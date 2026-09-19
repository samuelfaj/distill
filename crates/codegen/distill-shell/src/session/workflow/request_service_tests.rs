// Modified for Distill by Samuel Fajreldines, 2026.
use std::time::Duration;

use distill_tools::implementations::distill::workflow::{
    WorkflowControl, WorkflowLaunchAck, WorkflowLaunchRequest, WorkflowSource, WorkflowToolInput,
};
use tokio::sync::{mpsc, oneshot};

use super::{RequestServiceOptions, spawn_request_service};
use crate::session::workflow::manager::WorkflowManager;
use crate::session::workflow::tracker::WorkflowRunStatus;

async fn request(
    tx: &mpsc::UnboundedSender<
        distill_tools::implementations::distill::workflow::WorkflowLaunchEnvelope,
    >,
    source: WorkflowSource,
) -> WorkflowLaunchAck {
    let (ack_tx, ack_rx) = oneshot::channel();
    let input = WorkflowToolInput {
        source,
        agent_budget: None,
        args: None,
        validate_only: false,
    };
    tx.send((WorkflowLaunchRequest { input }, ack_tx)).unwrap();
    tokio::time::timeout(Duration::from_secs(2), ack_rx)
        .await
        .expect("ack within timeout")
        .expect("ack sent")
}

#[tokio::test]
async fn control_requests_stop_by_name_and_reject_inapplicable_or_unknown_runs() {
    let (manager, tracker) = WorkflowManager::test_bundle();
    let run_id = "wf_ctrl".to_owned();
    tracker.lock().start_run(
        run_id.clone(),
        "review-changes".into(),
        "obj".into(),
        Vec::new(),
        None,
        None,
    );
    let (_done_tx, done_rx) = oneshot::channel();
    manager
        .lock()
        .await
        .test_insert_active_run(run_id.clone(), done_rx);
    let (tx, rx) = mpsc::unbounded_channel();
    spawn_request_service(
        rx,
        RequestServiceOptions {
            manager,
            cwd: std::env::temp_dir(),
            session_dir: std::env::temp_dir(),
            enabled: true,
        },
    );

    let unknown = request(
        &tx,
        WorkflowSource::Stop {
            run_id: "nope".into(),
        },
    )
    .await;
    assert!(
        matches!(
            unknown,
            WorkflowLaunchAck::Rejected {
                code: "workflow_control_unknown_run",
                ..
            }
        ),
        "{unknown:?}"
    );

    let stopped = request(
        &tx,
        WorkflowSource::Stop {
            run_id: "review-changes".into(),
        },
    )
    .await;
    assert!(
        matches!(
            &stopped,
            WorkflowLaunchAck::Controlled { run_id: id, name, control: WorkflowControl::Stop }
                if id == &run_id && name == "review-changes"
        ),
        "{stopped:?}"
    );
    let stopped_state = tracker.lock().get(&run_id).unwrap();
    assert_eq!(stopped_state.status, WorkflowRunStatus::Cancelled);
    assert!(
        !tracker
            .lock()
            .is_unreported_completion(&run_id, stopped_state.revision),
        "a tool-initiated stop must not queue a completion wake turn"
    );

    let paused = request(
        &tx,
        WorkflowSource::Pause {
            run_id: run_id.clone(),
        },
    )
    .await;
    assert!(
        matches!(
            &paused,
            WorkflowLaunchAck::Rejected { code: "workflow_control_not_applicable", detail }
                if detail == "run 'review-changes' is cancelled and cannot be paused"
        ),
        "{paused:?}"
    );
}

#[tokio::test]
async fn requests_are_rejected_while_workflows_are_disabled() {
    let (manager, _tracker) = WorkflowManager::test_bundle();
    let (tx, rx) = mpsc::unbounded_channel();
    spawn_request_service(
        rx,
        RequestServiceOptions {
            manager,
            cwd: std::env::temp_dir(),
            session_dir: std::env::temp_dir(),
            enabled: false,
        },
    );
    let ack = request(
        &tx,
        WorkflowSource::Stop {
            run_id: "wf_any".into(),
        },
    )
    .await;
    assert!(
        matches!(
            ack,
            WorkflowLaunchAck::Rejected {
                code: "workflows_disabled",
                ..
            }
        ),
        "{ack:?}"
    );
}
