// Modified for Distill by Samuel Fajreldines, 2026.
//! When a write or edit tool touches a project `.grok/workflows/*.rhai` script, run the workflow validator on it and warn the model on failure.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use distill_tools::types::tool::ToolKind;

/// Canonical path fields on write and edit tools.
/// Client-facing names come from `${{ params.<kind>.<param> }}` via [`path_param_names_for_kind`].
const CANONICAL_PATH_PARAMS: &[&str] = &["file_path", "path", "target_file"];

const CHECK_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const MAX_CONCURRENT_CHECKS: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct WorkflowSmokeCheckFailure {
    pub(super) path: PathBuf,
    pub(super) detail: String,
}

#[derive(Debug)]
pub(super) struct AuthoredWorkflowSnapshot {
    path: PathBuf,
    script: String,
}

pub(super) fn is_project_workflow_rhai_path(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("rhai") {
        return false;
    }

    let components: Vec<_> = path.components().collect();
    components.windows(2).any(|pair| {
        let [a, b] = pair else { return false };
        matches!(a, Component::Normal(name) if *name == ".grok")
            && matches!(b, Component::Normal(name) if *name == "workflows")
    })
}

pub(super) fn path_param_names_for_kind(
    kind: ToolKind,
    param_names: &HashMap<ToolKind, HashMap<String, String>>,
) -> Vec<String> {
    param_names
        .get(&kind)
        .map(|map| {
            CANONICAL_PATH_PARAMS
                .iter()
                .filter_map(|canonical| map.get(*canonical).cloned())
                .collect()
        })
        .filter(|names: &Vec<String>| !names.is_empty())
        .unwrap_or_else(|| {
            CANONICAL_PATH_PARAMS
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        })
}

pub(super) fn workflow_param_name(
    canonical: &str,
    param_names: &HashMap<ToolKind, HashMap<String, String>>,
) -> String {
    param_names
        .get(&ToolKind::Workflow)
        .and_then(|map| map.get(canonical))
        .cloned()
        .unwrap_or_else(|| canonical.to_owned())
}

pub(super) fn authored_workflow_arg_path<'a>(
    tool_kind: Option<ToolKind>,
    args: &'a serde_json::Value,
    path_param_names: &[String],
) -> Option<&'a str> {
    if !matches!(tool_kind, Some(ToolKind::Write) | Some(ToolKind::Edit)) {
        return None;
    }
    let input = path_param_names
        .iter()
        .find_map(|key| args.get(key)?.as_str())?;
    is_project_workflow_rhai_path(Path::new(input)).then_some(input)
}

/// Last model-emitted write or edit per workflow path.
/// Earlier edits in the same batch are not smoke-checked; only the final file state matters.
pub(super) fn last_smoke_check_indices<I>(targets: I) -> HashSet<usize>
where
    I: IntoIterator<Item = (usize, Option<String>)>,
{
    let mut last = HashMap::new();
    for (idx, path) in targets {
        if let Some(path) = path {
            last.insert(path, idx);
        }
    }
    last.into_values().collect()
}

pub(super) async fn snapshot_authored_workflow(
    tool_kind: Option<ToolKind>,
    args: &serde_json::Value,
    path_param_names: &[String],
    cwd: &Path,
    display_cwd: Option<&Path>,
    session_dir: &Path,
) -> Option<Result<AuthoredWorkflowSnapshot, WorkflowSmokeCheckFailure>> {
    if !matches!(tool_kind, Some(ToolKind::Write) | Some(ToolKind::Edit)) {
        return None;
    }

    let input = path_param_names
        .iter()
        .find_map(|key| args.get(key)?.as_str())?;
    let input_path = Path::new(input);
    if !is_project_workflow_rhai_path(input_path) {
        return None;
    }

    let path = distill_tools::types::resources::resolve_model_path(cwd, display_cwd, input);
    let resolution_path = path.clone();
    let cwd = cwd.to_path_buf();
    let session_dir = session_dir.to_path_buf();
    let resolution = tokio::task::spawn_blocking(move || {
        crate::session::workflow::registry::resolve_by_path(
            &resolution_path,
            &cwd,
            Some(&session_dir),
        )
    });
    let resolution = match tokio::time::timeout(CHECK_TIMEOUT, resolution).await {
        Ok(join) => join,
        Err(_) => {
            return Some(Err(WorkflowSmokeCheckFailure {
                path,
                detail: format!("smoke check exceeded {} ms", CHECK_TIMEOUT.as_millis()),
            }));
        }
    };

    Some(match resolution {
        Ok(Ok(resolved)) => Ok(AuthoredWorkflowSnapshot {
            path,
            script: resolved.script,
        }),
        Ok(Err(error)) => Err(WorkflowSmokeCheckFailure {
            path,
            detail: error.to_string(),
        }),
        Err(error) => Err(WorkflowSmokeCheckFailure {
            path,
            detail: format!("smoke-check resolution task failed: {error}"),
        }),
    })
}

struct CancelOnDrop(tokio_util::sync::CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn check_snapshot(
    snapshot: AuthoredWorkflowSnapshot,
    permits: &Arc<tokio::sync::Semaphore>,
) -> Option<WorkflowSmokeCheckFailure> {
    let path = snapshot.path;
    let permit = Arc::clone(permits).acquire_owned().await.ok()?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let validation_cancel = cancel.clone();
    let validation = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        distill_workflow::validate_script_with_cancel(&snapshot.script, None, validation_cancel)
            .map_err(|error| error.to_string())
    });
    let validation = match tokio::time::timeout(CHECK_TIMEOUT, validation).await {
        Ok(result) => Ok(result.ok()),
        Err(error) => {
            cancel.cancel();
            Err(error)
        }
    };

    let detail = match validation {
        Ok(Some(Ok(_))) => return None,
        Ok(Some(Err(error))) => error,
        Ok(None) => "smoke-check task failed".to_owned(),
        Err(_) => format!("smoke check exceeded {} ms", CHECK_TIMEOUT.as_millis()),
    };
    Some(WorkflowSmokeCheckFailure { path, detail })
}

pub(super) fn append_validation_warning(
    prompt_text: &mut String,
    failure: &WorkflowSmokeCheckFailure,
    workflow_tool_name: &str,
    script_path_param: &str,
    validate_only_param: &str,
) {
    let path = failure.path.display().to_string();
    let quoted_path = serde_json::to_string(&path).unwrap_or_else(|_| format!("\"{path}\""));
    prompt_text.push_str(&format!(
        "\n\nWarning: The current workflow fails smoke checks. \
         To validate, run {workflow_tool_name}({script_path_param}={quoted_path}, {validate_only_param}=true). \
         Avoid launching the workflow before validation passes."
    ));
}

#[cfg(test)]
#[path = "workflow_write_smoke_check_tests.rs"]
mod tests;
