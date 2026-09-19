//! Cross-cutting reminder: notifies LSP of file changes and drains diagnostics.

use std::path::PathBuf;
use std::sync::Arc;

use crate::implementations::lsp::{DiskChangeKind, LspBackend};
use crate::types::output::{
    ApplyPatchFileResult, ApplyPatchOutput, SearchReplaceOutput, ToolOutput,
};
use crate::types::resources::SharedResources;
use crate::types::tool::Reminder;

pub struct LspDiagnosticsReminder;

#[async_trait::async_trait]
impl Reminder for LspDiagnosticsReminder {
    async fn collect_reminders(
        &self,
        resources: SharedResources,
        tool_output: &ToolOutput,
    ) -> Vec<String> {
        let lsp = {
            let res = resources.lock().await;
            match res.get::<Arc<dyn LspBackend>>() {
                Some(h) => h.clone(),
                None => return vec![],
            }
        };

        lsp.ensure_started_background();

        // Structured mutations we ourselves made. bash/git have no file list;
        // watching the workspace for those is the OS-watcher leak this path
        // exists to avoid.
        for (path, content, kind) in disk_events(tool_output) {
            lsp.notify_file_event(&path, content.as_deref(), kind).await;
        }

        // Drain any pending diagnostics (from this or previous edits).
        if let Some(summary) = lsp
            .drain_diagnostics(crate::implementations::lsp::DIAGNOSTICS_DRAIN_TIMEOUT)
            .await
        {
            return vec![summary.text];
        }

        vec![]
    }
}

fn disk_events(tool_output: &ToolOutput) -> Vec<(PathBuf, Option<String>, DiskChangeKind)> {
    match tool_output {
        ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(edits)) => {
            let kind = if edits.old_string.is_empty() {
                DiskChangeKind::Created
            } else {
                DiskChangeKind::Changed
            };
            let content = std::fs::read_to_string(&edits.absolute_path).ok();
            vec![(edits.absolute_path.clone(), content, kind)]
        }
        ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => {
            files.iter().flat_map(apply_patch_events).collect()
        }
        _ => Vec::new(),
    }
}

fn apply_patch_events(
    file: &ApplyPatchFileResult,
) -> Vec<(PathBuf, Option<String>, DiskChangeKind)> {
    match file.action.as_str() {
        "added" => vec![(
            file.path.clone(),
            Some(file.new_text.clone()),
            DiskChangeKind::Created,
        )],
        "deleted" => vec![(file.path.clone(), None, DiskChangeKind::Deleted)],
        "moved" => {
            let mut events = vec![(file.path.clone(), None, DiskChangeKind::Deleted)];
            if let Some(dest) = &file.move_to {
                events.push((
                    dest.clone(),
                    Some(file.new_text.clone()),
                    DiskChangeKind::Created,
                ));
            }
            events
        }
        _ => vec![(
            file.path.clone(),
            Some(file.new_text.clone()),
            DiskChangeKind::Changed,
        )],
    }
}
