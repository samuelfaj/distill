// Modified for Distill by Samuel Fajreldines, 2026.
use pretty_assertions::assert_eq;

use super::*;
use crate::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
use crate::implementations::distill::task::coordinator::{
    ActiveMessageAdmission, ChildCompletion, ChildControl, ChildRunOutput, ChildRunRequest,
    ChildRunner, SendBoxFuture, StartedChild, SubagentCoordinator, SubagentCoordinatorReceiver,
    SubagentProgress,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageDelivery, ActiveAgentMessageOutcome, ActiveMessageRoute, AgentMessageSender,
    AgentMessageSenderResource, MAX_ACTIVE_AGENT_MESSAGE_BYTES, SubagentDepthCounter,
    SubagentDescribeOutcome, SubagentOwner, SubagentRequest, SubagentValidateTypeOutcome,
};
use crate::types::resources::{Resources, SharedResources};
use crate::types::tool_metadata::test_ctx;

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn completes<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("send_subagent_message test operation timed out")
}

struct ToolTestControl {
    routes: Option<tokio::sync::mpsc::UnboundedSender<ActiveMessageRoute>>,
}

impl ChildControl for ToolTestControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(SubagentProgress::default())
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let route = delivery.route();
        let routes = self.routes.clone();
        let admitted =
            delivery.commit_admission(|| routes.is_none_or(|routes| routes.send(route).is_ok()));
        Box::pin(std::future::ready(if admitted == Some(true) {
            ActiveMessageAdmission::Admitted
        } else {
            ActiveMessageAdmission::Rejected
        }))
    }

    fn cancel(&self) {}
}

struct ToolTestRunner {
    senders: tokio::sync::mpsc::UnboundedSender<(String, Option<AgentMessageSender>)>,
    routes: Option<tokio::sync::mpsc::UnboundedSender<ActiveMessageRoute>>,
}

impl ChildRunner for ToolTestRunner {
    type Control = ToolTestControl;
    type RootControl = crate::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let senders = self.senders.clone();
        let routes = self.routes.clone();
        Box::pin(async move {
            let id = run.request.id.clone();
            let sender = run.agent_message_sender;
            let _ = run
                .reporter
                .started(StartedChild {
                    child_session_id: id.clone(),
                    persona: None,
                    resumed_from: None,
                    child_cwd: String::new(),
                    worktree_path: None,
                    effective_model_id: "test-model".to_owned(),
                    definition_background: false,
                    control: ToolTestControl { routes },
                })
                .await;
            let _ = senders.send((id, sender));
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

    fn on_completed(
        &self,
        _: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        terminal_published();
    }
}

fn coordinator_backend() -> (ChannelBackend, SubagentCoordinatorReceiver) {
    let (sender, receiver) = SubagentCoordinator::<ToolTestRunner>::channel();
    (
        ChannelBackend::for_coordinator_session(sender, "trusted-parent"),
        receiver,
    )
}

fn resources_with_backend(backend: ChannelBackend) -> Resources {
    let mut resources = Resources::new();
    resources.insert(backend.into_resource());
    resources.insert(SubagentDepthCounter(0));
    resources
}

async fn run_with_delivery(
    resources: SharedResources,
    subagent_id: &str,
    text: String,
    delivery: Option<SendSubagentMessageDelivery>,
    queue: bool,
) -> SendSubagentMessageOutput {
    completes(distill_tool_runtime::Tool::run(
        &SendSubagentMessageTool,
        test_ctx(resources),
        SendSubagentMessageInput {
            subagent_id: subagent_id.to_owned(),
            text,
            delivery,
            queue,
        },
    ))
    .await
    .unwrap()
}

async fn run(
    resources: SharedResources,
    subagent_id: &str,
    text: String,
) -> SendSubagentMessageOutput {
    run_with_delivery(resources, subagent_id, text, None, false).await
}

async fn backend_operation(
    delivery: Option<SendSubagentMessageDelivery>,
    queue: bool,
) -> ActiveAgentMessageOperation {
    let (backend, mut receiver) = coordinator_backend();
    let send = run_with_delivery(
        resources_with_backend(backend).into_shared(),
        "sub-1",
        "follow up".to_owned(),
        delivery,
        queue,
    );
    let respond = async move {
        let ingress = completes(receiver.active_messages.recv())
            .await
            .expect("expected active-message ingress");
        let operation = ingress.request.request.operation();
        ingress
            .request
            .respond_to
            .send(ActiveAgentMessageOutcome::Accepted {
                message_id: "message-1".to_owned(),
            })
            .unwrap();
        operation
    };
    completes(async { tokio::join!(send, respond) }).await.1
}

fn request(id: &str, parent_session_id: &str, owner: SubagentOwner) -> SubagentRequest {
    SubagentRequest {
        id: id.to_owned(),
        prompt: "work".to_owned(),
        description: "test child".to_owned(),
        subagent_type: "general-purpose".to_owned(),
        parent_session_id: parent_session_id.to_owned(),
        parent_prompt_id: Some("prompt".to_owned()),
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
    }
}

async fn run_backend_outcome(outcome: ActiveAgentMessageOutcome) -> SendSubagentMessageOutput {
    let (backend, mut receiver) = coordinator_backend();
    let send = run(
        resources_with_backend(backend).into_shared(),
        "sub-1",
        "follow up".to_owned(),
    );
    let respond = async move {
        let ingress = completes(receiver.active_messages.recv())
            .await
            .expect("expected active-message ingress");
        ingress.request.respond_to.send(outcome).unwrap();
    };
    completes(async { tokio::join!(send, respond) }).await.0
}

#[test]
fn required_input_keys_are_semantically_pinned() {
    let schema = crate::registry::types::generate_schema::<SendSubagentMessageInput>();
    let Some(required_keys) = schema.get("required").and_then(|v| v.as_array()) else {
        panic!("schema missing required: {schema}");
    };
    let mut required = required_keys
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    required.sort_unstable();
    assert_eq!(required, ["subagent_id", "text"]);
    assert!(schema.pointer("/properties/queue").is_none());
    assert!(
        schema
            .pointer("/properties/delivery")
            .is_some_and(serde_json::Value::is_object)
    );
}

#[test]
fn delivery_values_are_a_closed_snake_case_set() {
    let values = [
        SendSubagentMessageDelivery::Steer,
        SendSubagentMessageDelivery::Queue,
        SendSubagentMessageDelivery::Interject,
    ]
    .map(|value| serde_json::to_value(value).expect("serialize delivery"));
    assert_eq!(values, ["steer", "queue", "interject"]);
}

#[test]
fn unknown_delivery_value_fails_to_deserialize() {
    let error = serde_json::from_value::<SendSubagentMessageInput>(serde_json::json!({
        "subagent_id": "sub-1",
        "text": "follow up",
        "delivery": "urgent",
    }))
    .expect_err("an unknown delivery must fail closed");
    assert!(
        error.to_string().contains("unknown variant `urgent`"),
        "{error}"
    );
}

#[test]
fn tool_capabilities_are_write_scoped() {
    let capabilities = distill_tool_runtime::Tool::capabilities(&SendSubagentMessageTool);
    assert!(!capabilities.is_read_only);
    assert_eq!(
        capabilities.tool_scope,
        Some(distill_tool_protocol::ToolScope::Write)
    );
}

#[tokio::test]
async fn accepted_roundtrip_uses_backend_bound_parent_and_preserves_request() {
    let (backend, mut receiver) = coordinator_backend();
    let send = run(
        resources_with_backend(backend).into_shared(),
        "sub-1",
        "follow up".to_owned(),
    );
    let respond = async move {
        let ingress = completes(receiver.active_messages.recv())
            .await
            .expect("expected active-message ingress");
        assert!(matches!(
            &ingress.request.sender_context,
            crate::implementations::distill::task::types::ActiveMessageSenderContext::RootSession {
                session_id
            } if session_id.as_ref() == "trusted-parent"
        ));
        assert!(matches!(
            ingress.request.request.target(),
            crate::implementations::distill::task::types::ActiveMessageTarget::ChildId(id)
                if id == "sub-1"
        ));
        assert_eq!(ingress.request.request.text().as_ref(), "follow up");
        assert_eq!(
            ingress.request.request.operation(),
            crate::implementations::distill::task::types::ActiveAgentMessageOperation::Steer
        );
        ingress
            .request
            .respond_to
            .send(ActiveAgentMessageOutcome::Accepted {
                message_id: "message-1".to_owned(),
            })
            .unwrap();
    };

    assert_eq!(
        completes(async { tokio::join!(send, respond) }).await.0,
        SendSubagentMessageOutput::Accepted {
            message_id: "message-1".to_owned()
        }
    );
}

#[tokio::test]
async fn legacy_queue_true_without_delivery_maps_to_queue() {
    assert_eq!(
        ActiveAgentMessageOperation::Queue,
        backend_operation(None, true).await
    );
}

#[tokio::test]
async fn explicit_delivery_wins_over_legacy_queue() {
    assert_eq!(
        ActiveAgentMessageOperation::Steer,
        backend_operation(Some(SendSubagentMessageDelivery::Steer), true).await
    );
}

#[tokio::test]
async fn delivery_interject_reaches_the_backend_as_interject() {
    assert_eq!(
        ActiveAgentMessageOperation::Interject,
        backend_operation(Some(SendSubagentMessageDelivery::Interject), false).await
    );
}

#[tokio::test]
async fn invalid_message_sizes_return_limit_without_calling_backend() {
    let (backend, mut receiver) = coordinator_backend();
    let resources = resources_with_backend(backend).into_shared();

    for (text, expected_observed_bytes) in [
        (String::new(), 0),
        (
            "x".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES + 1),
            MAX_ACTIVE_AGENT_MESSAGE_BYTES + 1,
        ),
    ] {
        assert_eq!(
            run(resources.clone(), "sub-1", text).await,
            SendSubagentMessageOutput::Limit {
                max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
                observed_bytes: expected_observed_bytes,
            }
        );
        assert!(receiver.active_messages.try_recv().is_err());
    }
}

#[tokio::test]
async fn explicit_root_depth_without_backend_returns_unsupported() {
    let mut resources = Resources::new();
    resources.insert(SubagentDepthCounter(0));
    assert_eq!(
        run(resources.into_shared(), "sub-1", "follow up".to_owned()).await,
        SendSubagentMessageOutput::Unsupported
    );
}

#[tokio::test]
async fn missing_or_ungranted_nested_depth_returns_unsupported_without_calling_backend() {
    for depth in [None, Some(1)] {
        let (backend, mut receiver) = coordinator_backend();
        let mut resources = Resources::new();
        resources.insert(backend.into_resource());
        if let Some(depth) = depth {
            resources.insert(SubagentDepthCounter(depth));
        }

        assert_eq!(
            run(resources.into_shared(), "sub-1", "follow up".to_owned()).await,
            SendSubagentMessageOutput::Unsupported
        );
        assert!(receiver.active_messages.try_recv().is_err());
    }
}

#[tokio::test]
async fn child_sender_parses_targets_at_any_depth() {
    let (coordinator_sender, receiver) = SubagentCoordinator::<ToolTestRunner>::channel();
    let (senders, mut sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::from_channel(
        receiver,
        ToolTestRunner {
            senders,
            routes: None,
        },
        Default::default(),
    );
    let coordinator_task = tokio::spawn(coordinator.run());
    let backend = ChannelBackend::from_coordinator(coordinator_sender);
    let child_id = uuid::Uuid::now_v7().to_string();
    let spawn = tokio::spawn({
        let backend = backend.clone();
        let child_id = child_id.clone();
        async move {
            backend
                .spawn(request(&child_id, "root", SubagentOwner::Task), None)
                .await
        }
    });
    let (_, sender) = completes(sender_rx.recv()).await.expect("child sender");
    let mut resources = Resources::new();
    resources.insert(SubagentDepthCounter(0));
    resources.insert(AgentMessageSenderResource(
        sender.expect("granted child sender"),
    ));
    let resources = resources.into_shared();

    assert_eq!(
        run(resources.clone(), "parent", "follow up".to_owned()).await,
        SendSubagentMessageOutput::Unsupported,
    );
    assert_eq!(
        run_with_delivery(
            resources.clone(),
            &child_id,
            "follow up".to_owned(),
            Some(SendSubagentMessageDelivery::Interject),
            false,
        )
        .await,
        SendSubagentMessageOutput::NotFoundOrNotOwned,
    );
    let error = completes(distill_tool_runtime::Tool::run(
        &SendSubagentMessageTool,
        test_ctx(resources),
        SendSubagentMessageInput {
            subagent_id: "not-an-agent-id".to_owned(),
            text: "follow up".to_owned(),
            delivery: None,
            queue: false,
        },
    ))
    .await
    .expect_err("invalid child target must fail input parsing");
    assert_eq!(
        error.kind,
        distill_tool_runtime::ToolErrorKind::InvalidArguments
    );
    assert_eq!(
        error.detail,
        "subagent_id must be `parent` or a valid agent ID"
    );

    coordinator_task.abort();
    spawn.abort();
}

#[tokio::test]
async fn depth_zero_child_sender_uses_granted_path_not_backend() {
    let (coordinator_sender, receiver) = SubagentCoordinator::<ToolTestRunner>::channel();
    let (senders, mut sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::from_channel(
        receiver,
        ToolTestRunner {
            senders,
            routes: None,
        },
        Default::default(),
    );
    let coordinator_task = tokio::spawn(coordinator.run());
    let backend = ChannelBackend::from_coordinator(coordinator_sender);
    let child_id = uuid::Uuid::now_v7().to_string();
    let spawn = tokio::spawn({
        let backend = backend.clone();
        let child_id = child_id.clone();
        async move {
            backend
                .spawn(request(&child_id, "root", SubagentOwner::Task), None)
                .await
        }
    });
    let (_, sender) = completes(sender_rx.recv()).await.expect("child sender");
    let (legacy, mut legacy_rx) = coordinator_backend();
    let mut resources = resources_with_backend(legacy);
    resources.insert(AgentMessageSenderResource(
        sender.expect("granted child sender"),
    ));

    assert_eq!(
        run(resources.into_shared(), &child_id, "follow up".to_owned()).await,
        SendSubagentMessageOutput::NotFoundOrNotOwned,
    );
    assert!(legacy_rx.active_messages.try_recv().is_err());

    coordinator_task.abort();
    spawn.abort();
}

#[tokio::test]
async fn child_tool_routes_parent_and_sibling_through_coordinator() {
    let (coordinator_sender, receiver) = SubagentCoordinator::<ToolTestRunner>::channel();
    let (senders, mut sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let (routes, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::from_channel(
        receiver,
        ToolTestRunner {
            senders,
            routes: Some(routes),
        },
        Default::default(),
    );
    let coordinator_task = tokio::spawn(coordinator.run());
    let root_backend = ChannelBackend::from_coordinator(coordinator_sender.clone());
    let parent_id = uuid::Uuid::now_v7().to_string();
    let sibling_id = uuid::Uuid::now_v7().to_string();
    let child_id = uuid::Uuid::now_v7().to_string();
    let mut spawns = Vec::new();
    for id in [&parent_id, &sibling_id] {
        spawns.push(tokio::spawn({
            let backend = root_backend.clone();
            let id = id.clone();
            async move {
                backend
                    .spawn(request(&id, "root", SubagentOwner::Task), None)
                    .await
            }
        }));
        let _ = completes(sender_rx.recv())
            .await
            .expect("root child sender");
    }
    let nested_backend =
        ChannelBackend::for_coordinator_session(coordinator_sender, parent_id.clone());
    spawns.push(tokio::spawn({
        let child_id = child_id.clone();
        async move {
            nested_backend
                .spawn(request(&child_id, "ignored", SubagentOwner::Task), None)
                .await
        }
    }));
    let (_, sender) = completes(sender_rx.recv())
        .await
        .expect("nested child sender");
    let mut resources = Resources::new();
    resources.insert(SubagentDepthCounter(2));
    resources.insert(AgentMessageSenderResource(
        sender.expect("granted child sender"),
    ));
    let resources = resources.into_shared();

    assert!(matches!(
        run(resources.clone(), "parent", "up".to_owned()).await,
        SendSubagentMessageOutput::Accepted { .. }
    ));
    assert_eq!(
        completes(route_rx.recv()).await,
        Some(ActiveMessageRoute::DescendantToParent)
    );
    assert!(matches!(
        run(resources, &sibling_id, "across".to_owned()).await,
        SendSubagentMessageOutput::Accepted { .. }
    ));
    assert_eq!(
        completes(route_rx.recv()).await,
        Some(ActiveMessageRoute::Peer)
    );

    coordinator_task.abort();
    for spawn in spawns {
        spawn.abort();
    }
}

#[tokio::test]
async fn legacy_ingress_is_unsupported_without_sending_an_event() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let resources =
        resources_with_backend(ChannelBackend::for_session(tx, "trusted-parent")).into_shared();

    assert_eq!(
        run(resources, "sub-1", "follow up".to_owned()).await,
        SendSubagentMessageOutput::Unsupported
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn coordinator_outcomes_map_to_closed_tool_outputs() {
    for (outcome, expected) in [
        (
            ActiveAgentMessageOutcome::NotFoundOrNotOwned,
            SendSubagentMessageOutput::NotFoundOrNotOwned,
        ),
        (
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            SendSubagentMessageOutput::NotActiveOrFinalizing,
        ),
        (
            ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
            SendSubagentMessageOutput::NotAcceptedBeforeDeadline,
        ),
        (
            ActiveAgentMessageOutcome::Unsupported,
            SendSubagentMessageOutput::Unsupported,
        ),
        (
            ActiveAgentMessageOutcome::Saturated { max_in_flight: 64 },
            SendSubagentMessageOutput::Saturated { max_in_flight: 64 },
        ),
        (
            ActiveAgentMessageOutcome::Limit {
                max_bytes: 7,
                observed_bytes: 9,
            },
            SendSubagentMessageOutput::Limit {
                max_bytes: 7,
                observed_bytes: 9,
            },
        ),
    ] {
        assert_eq!(run_backend_outcome(outcome).await, expected);
    }
}

#[tokio::test]
async fn dropped_coordinator_response_maps_to_definite_channel_closure() {
    let (backend, mut receiver) = coordinator_backend();
    let send = run(
        resources_with_backend(backend).into_shared(),
        "sub-1",
        "follow up".to_owned(),
    );
    let drop_response = async move {
        let ingress = completes(receiver.active_messages.recv())
            .await
            .expect("expected active-message ingress");
        drop(ingress);
    };

    assert_eq!(
        completes(async { tokio::join!(send, drop_response) })
            .await
            .0,
        SendSubagentMessageOutput::ChannelClosed
    );
}

#[test]
fn disposition_classification_is_closed() {
    use SendSubagentMessageDisposition as Disposition;

    for (output, expected) in [
        (
            SendSubagentMessageOutput::Accepted {
                message_id: "message-1".into(),
            },
            Disposition::Accepted,
        ),
        (
            SendSubagentMessageOutput::NotFoundOrNotOwned,
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::NotActiveOrFinalizing,
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::Saturated { max_in_flight: 8 },
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::NotAcceptedBeforeDeadline,
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::Unsupported,
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::Limit {
                max_bytes: 8,
                observed_bytes: 9,
            },
            Disposition::Rejected,
        ),
        (
            SendSubagentMessageOutput::AdmissionUncertain,
            Disposition::Unconfirmed,
        ),
        (
            SendSubagentMessageOutput::ChannelClosed,
            Disposition::Rejected,
        ),
    ] {
        assert_eq!(output.disposition(), expected);
    }
}

#[test]
fn uncertain_display_string_does_not_claim_failure_or_success() {
    let display = SendSubagentMessageOutput::AdmissionUncertain
        .to_string()
        .to_ascii_lowercase();
    assert!(display.contains("could not be confirmed"));
    assert!(display.contains("may or may not have been accepted"));
    assert!(!display.contains("failed"));
    assert!(!display.contains("succeeded"));
}

#[test]
fn proved_rejection_strings_do_not_claim_uncertainty() {
    for output in [
        SendSubagentMessageOutput::NotAcceptedBeforeDeadline,
        SendSubagentMessageOutput::ChannelClosed,
    ] {
        let display = output.to_string().to_ascii_lowercase();
        assert!(display.contains("not accepted"));
        assert!(!display.contains("may or may not"));
    }
}

#[tokio::test]
async fn closed_coordinator_ingress_maps_to_channel_closed() {
    let (backend, receiver) = coordinator_backend();
    drop(receiver);

    assert_eq!(
        run(
            resources_with_backend(backend).into_shared(),
            "sub-1",
            "follow up".to_owned(),
        )
        .await,
        SendSubagentMessageOutput::ChannelClosed
    );
}
