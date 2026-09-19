// Modified for Distill by Samuel Fajreldines, 2026.
use crate::app::actions::Effect;
use crate::app::app_view::AppView;

/// Open the interactive Claude-import modal on the welcome screen.
///
/// When the scan finds nothing importable, records the dismissal and shows an info warning instead of the modal.
pub(super) fn dispatch_import_claude(app: &mut AppView) -> Vec<Effect> {
    let cwd = app.cwd.clone();
    let plan = distill_shell::claude_import::scan_importable_settings(&cwd);

    if plan.is_empty() {
        distill_shell::claude_import_state::mark_dismissed(&cwd);
        // Always write the [claude_compat] imported = true marker so the user's opt-in is recorded even on an empty plan
        if let Err(e) = distill_shell::claude_import::mark_claude_imported() {
            tracing::warn!(error = %e, "Failed to write Claude import marker");
        }
        app.has_claude_import = false;
        app.startup_warnings
            .retain(|w| !w.message.contains("Claude settings"));
        app.startup_warnings.push(crate::startup::StartupWarning {
            severity: crate::startup::WarningSeverity::Info,
            message: "No Claude settings found to import.".into(),
            action: None,
        });
        return vec![];
    }

    app.import_claude_modal =
        Some(crate::views::import_claude_modal::ImportClaudeModalState::new(plan, cwd));
    vec![]
}

/// Apply the user's selection from the import modal and close it.
pub(super) fn dispatch_import_claude_confirm(app: &mut AppView) -> Vec<Effect> {
    let Some(modal) = app.import_claude_modal.take() else {
        return vec![];
    };
    let cwd = modal.cwd.clone();
    let total_in_modal = modal.total_count();
    let filtered = modal.filtered_plan();
    let selected_count = filtered.global_items.len() + filtered.project_items.len();

    let mut summary = if selected_count == 0 {
        "No items selected.".to_string()
    } else {
        filtered.summary(&cwd).trim_end().to_string()
    };

    if selected_count > 0 {
        match distill_shell::claude_import::apply_import(&filtered, &cwd) {
            Ok(result) => {
                summary.push_str(&format!(
                    "\nImported {} of {} setting(s).",
                    result.total(),
                    total_in_modal
                ));
                for path in &result.modified_files {
                    summary.push_str(&format!("\n  Updated: {}", path));
                }
            }
            Err(e) => {
                app.startup_warnings.push(crate::startup::StartupWarning {
                    severity: crate::startup::WarningSeverity::Warning,
                    message: format!("Failed to import Claude settings: {}", e),
                    action: None,
                });
                return vec![];
            }
        }
    }

    // Mark the current Claude state as seen so the startup warning won't re-fire for the same content
    // Skipped items remain importable via re-running the slash command
    distill_shell::claude_import_state::mark_imported(&cwd);
    if let Err(e) = distill_shell::claude_import::mark_claude_imported() {
        tracing::warn!(error = %e, "Failed to write Claude import marker");
    }
    app.has_claude_import = false;
    app.startup_warnings
        .retain(|w| !w.message.contains("Claude settings"));
    app.startup_warnings.push(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Info,
        message: summary,
        action: None,
    });
    vec![]
}

pub(super) fn dispatch_import_claude_cancel(app: &mut AppView) -> Vec<Effect> {
    app.import_claude_modal = None;
    vec![]
}

/// Hide the Claude-import menu row by recording the current `.claude/` content hash.
/// The startup detection compares the saved hash on the next launch; if it matches (no new Claude content), the menu stays hidden.
pub(super) fn dispatch_dismiss_claude_import(app: &mut AppView) -> Vec<Effect> {
    let cwd = app.cwd.clone();
    distill_shell::claude_import_state::mark_dismissed(&cwd);
    // The imported = true marker also stops the runtime fallbacks (perms, env, MCP servers, hooks, plugins) reading .claude/ and ~/.claude.json
    // Dismiss means "I've decided I want nothing from .claude/", so don't keep silently reading it at runtime
    if let Err(e) = distill_shell::claude_import::mark_claude_imported() {
        tracing::warn!(error = %e, "Failed to write Claude import marker on dismiss");
    }
    app.has_claude_import = false;
    // Reset the welcome menu selection: removing a row shifts indices
    // A stale selection (e.g. `Worktree mode` highlighted at index 1) would now point to a different row.
    app.welcome_menu_index = None;
    app.startup_warnings
        .retain(|w| !w.message.contains("Claude settings"));
    vec![]
}
