// Modified for Distill by Samuel Fajreldines, 2026.
//! Answers the workflow tool's requests (launch, validate, resume, pause,
//! stop) for one session. Each envelope carries a oneshot for the ack; a
//! dropped ack means the tool stopped waiting, so send failures are ignored.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use distill_tools::implementations::distill::workflow::{
    WorkflowControl, WorkflowLaunchAck, WorkflowLaunchEnvelope, WorkflowSource,
};
use tokio::sync::mpsc;

use super::manager::{ControlError, LaunchSpec, WorkflowManager};
use super::registry;

pub(crate) struct RequestServiceOptions {
    pub(crate) manager: Arc<tokio::sync::Mutex<WorkflowManager>>,
    pub(crate) cwd: PathBuf,
    pub(crate) session_dir: PathBuf,
    pub(crate) enabled: bool,
}

/// Runs until every `WorkflowLaunchHandle` sender is dropped.
pub(crate) fn spawn_request_service(
    mut rx: mpsc::UnboundedReceiver<WorkflowLaunchEnvelope>,
    options: RequestServiceOptions,
) {
    let RequestServiceOptions {
        manager,
        cwd,
        session_dir,
        enabled,
    } = options;
    tokio::spawn(async move {
        while let Some((req, ack)) = rx.recv().await {
            if !enabled {
                let _ = ack.send(WorkflowLaunchAck::Rejected {
                    code: "workflows_disabled",
                    detail: "Background workflows are disabled for this session \
                             ([workflows] enabled = false / GROK_WORKFLOWS=0 / remote flag)."
                        .into(),
                });
                continue;
            }
            let input = req.input;
            if let Err(detail) = input.validate() {
                let _ = ack.send(WorkflowLaunchAck::Rejected {
                    code: "workflow_invalid_input",
                    detail,
                });
                continue;
            }
            let mut resume_run_id = None;
            let resolved = match &input.source {
                WorkflowSource::Name { name } => {
                    registry::WorkflowRegistry::scan(Some(&cwd)).resolve_by_name(name)
                }
                WorkflowSource::Script { script } => registry::resolve_inline(script.clone()),
                WorkflowSource::ScriptPath { script_path } => {
                    registry::resolve_by_path(Path::new(script_path), &cwd, Some(&session_dir))
                }
                WorkflowSource::Resume { resume_from_run_id } => {
                    let resumable = {
                        let mgr = manager.lock().await;
                        let run_id = mgr.tracker().lock().find_run_id(resume_from_run_id);
                        run_id.and_then(|id| mgr.script_copy_for(&id).map(|script| (id, script)))
                    };
                    match resumable {
                        Some((run_id, script)) => {
                            resume_run_id = Some(run_id);
                            registry::resolve_inline(script)
                        }
                        None => {
                            let _ = ack.send(WorkflowLaunchAck::Rejected {
                                code: "workflow_resume_unknown_run",
                                detail: format!(
                                    "no workflow run with a persisted script matches \
                                     '{resume_from_run_id}' in this session"
                                ),
                            });
                            continue;
                        }
                    }
                }
                WorkflowSource::Pause { run_id } => {
                    let _ = ack.send(control_ack(&manager, run_id, WorkflowControl::Pause).await);
                    continue;
                }
                WorkflowSource::Stop { run_id } => {
                    let _ = ack.send(control_ack(&manager, run_id, WorkflowControl::Stop).await);
                    continue;
                }
            };
            let resolved = match resolved {
                Ok(r) => r,
                Err(e) => {
                    let _ = ack.send(WorkflowLaunchAck::Rejected {
                        code: "workflow_resolve_failed",
                        detail: e.to_string(),
                    });
                    continue;
                }
            };
            if input.validate_only {
                let script = resolved.script.clone();
                let probe_args = input.args.clone();
                let agent_budget = input
                    .agent_budget
                    .unwrap_or(distill_workflow::DEFAULT_AGENT_BUDGET);
                tokio::spawn(async move {
                    let verdict = tokio::task::spawn_blocking(move || {
                        distill_workflow::validate_script_with_agent_budget(
                            &script,
                            probe_args,
                            agent_budget,
                        )
                    })
                    .await;
                    let msg = match verdict {
                        Ok(Ok(report)) => WorkflowLaunchAck::Validated {
                            name: report.name,
                            phases: report.phases,
                            summary: report.outcome_summary,
                        },
                        Ok(Err(e)) => WorkflowLaunchAck::Rejected {
                            code: "workflow_validation_failed",
                            detail: e.to_string(),
                        },
                        Err(e) => WorkflowLaunchAck::Rejected {
                            code: "workflow_validation_failed",
                            detail: format!("validator panicked: {e}"),
                        },
                    };
                    let _ = ack.send(msg);
                });
                continue;
            }
            let definition_name = resolved.meta.name.clone();
            let args = match &resume_run_id {
                Some(rid) => manager.lock().await.args_copy_for(rid),
                None => input.args.clone().unwrap_or(serde_json::Value::Null),
            };
            let objective = args
                .get("objective")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| resolved.meta.description.clone());
            let spec = LaunchSpec {
                objective,
                args,
                agent_budget: input.agent_budget,
                effort: None,
                resume_run_id,
            };
            let launch_outcome = {
                let mut mgr = manager.lock().await;
                let result = mgr.launch(resolved, spec);
                let script_path = result
                    .as_ref()
                    .ok()
                    .and_then(|(run_id, _)| mgr.script_copy_path(run_id))
                    .map(|p| p.display().to_string());
                (result, script_path)
            };
            match launch_outcome {
                (Ok((run_id, outcome_rx)), script_path) => {
                    let display_name = manager
                        .lock()
                        .await
                        .tracker()
                        .lock()
                        .get(&run_id)
                        .map(|run| run.name.clone())
                        .unwrap_or(definition_name);
                    let _ = ack.send(WorkflowLaunchAck::Started {
                        task_id: run_id.clone(),
                        run_id: run_id.clone(),
                        name: display_name,
                        script_path,
                    });
                    tokio::spawn(async move {
                        if let Ok(outcome) = outcome_rx.await {
                            tracing::info!(run_id, ?outcome, "background workflow finished");
                        }
                    });
                }
                (Err(e), _) => {
                    let _ = ack.send(WorkflowLaunchAck::Rejected {
                        code: "workflow_launch_failed",
                        detail: e.to_string(),
                    });
                }
            }
        }
    });
}

async fn control_ack(
    manager: &tokio::sync::Mutex<WorkflowManager>,
    key: &str,
    control: WorkflowControl,
) -> WorkflowLaunchAck {
    let mut manager = manager.lock().await;
    match manager.control_run(key, control) {
        Ok(run) => {
            // The model gets the outcome from this tool result; a `/workflow stop`
            // relies on the completion wake to learn about it, so only the tool path opts out.
            if control == WorkflowControl::Stop {
                manager
                    .tracker()
                    .lock()
                    .mark_completion_reported(&run.run_id);
            }
            WorkflowLaunchAck::Controlled {
                run_id: run.run_id,
                name: run.name,
                control,
            }
        }
        Err(e @ ControlError::UnknownRun(_)) => WorkflowLaunchAck::Rejected {
            code: "workflow_control_unknown_run",
            detail: e.to_string(),
        },
        Err(e @ ControlError::NotApplicable { .. }) => WorkflowLaunchAck::Rejected {
            code: "workflow_control_not_applicable",
            detail: e.to_string(),
        },
    }
}

#[cfg(test)]
#[path = "request_service_tests.rs"]
mod tests;
