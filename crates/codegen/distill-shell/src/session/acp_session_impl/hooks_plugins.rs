// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

/// Path written (or the session key on auto-trust), never the raw git root.
fn hooks_trust_key(
    outcome: &distill_workspace::folder_trust::GrantOutcome,
    cwd: &std::path::Path,
) -> std::path::PathBuf {
    match outcome {
        distill_workspace::folder_trust::GrantOutcome::Granted { key, .. }
        | distill_workspace::folder_trust::GrantOutcome::AlreadyDurable { key } => key.clone(),
        distill_workspace::folder_trust::GrantOutcome::Refused { .. } => {
            distill_workspace::trust::workspace_key(cwd)
        }
    }
}

impl SessionActor {
    // ── Shared hook/plugin operation functions ────────────────────────

    /// Same as `--trust`: also allows repo-local MCP/LSP for this folder.
    /// Still requires a git worktree, but grants `workspace_key(cwd)` (the session key `decide` uses), not the raw git root.
    /// When `$HOME` is a checkout that root is UnsafeRoot and would look trusted without a record.
    pub(super) fn do_hooks_trust_project(cwd: &str) -> Result<std::path::PathBuf, String> {
        let cwd_path = std::path::Path::new(cwd);
        distill_workspace::session::git::find_git_root_from_path(cwd_path).map_err(|_| {
            "Not in a git repository. Project hooks require a git worktree root.".to_string()
        })?;
        let outcome = distill_workspace::folder_trust::grant_folder_trust(cwd_path);
        // Success only for this session key: durable / already-durable, or
        // auto-trust when cwd itself is unrecordable (cwd is $HOME / inert).
        if outcome.dismisses_gate() {
            return Ok(hooks_trust_key(&outcome, cwd_path));
        }
        Err(outcome.to_string())
    }

    /// Untrust the current project in the unified folder-trust store.
    /// Returns (session workspace key, was_trusted).
    pub(super) fn do_hooks_untrust_project(
        cwd: &str,
    ) -> Result<(std::path::PathBuf, bool), String> {
        let cwd_path = std::path::Path::new(cwd);
        distill_workspace::session::git::find_git_root_from_path(cwd_path)
            .map_err(|_| "Not in a git repository.".to_string())?;
        // Same key as grant: revoke_folder_trust runs workspace_key(cwd).
        let key = distill_workspace::trust::workspace_key(cwd_path);
        let was_trusted = crate::agent::folder_trust::revoke_folder_trust(cwd_path);
        Ok((key, was_trusted))
    }

    /// Re-resolve the repo `[mcp] max_output_bytes` cap for this session's cwd and update the toolset's `TruncationCfg` resource to match.
    /// Only that field changes, so any other `TruncationCfg` fields a host seeded are preserved.
    /// `resolve_max_mcp_output_bytes_for_cwd` is trust-gated, so calling this after a trust change keeps the seeded cap matching the gate.
    pub(super) async fn reseed_mcp_output_cap(&self) {
        let resolved = crate::util::config::resolve_max_mcp_output_bytes_for_cwd(
            std::path::Path::new(&self.session_info.cwd),
        );
        let bridge = std::sync::Arc::clone(self.agent.borrow().tool_bridge());
        let toolset = bridge.toolset();
        let mut resources = toolset.resources.lock().await;
        let existing = resources
            .get::<distill_tools::types::resources::TruncationCfg>()
            .map(|c| c.0.clone());
        match (resolved, existing) {
            (resolved, Some(mut cfg)) => {
                if cfg.mcp_max_output_bytes != resolved {
                    cfg.mcp_max_output_bytes = resolved;
                    resources.insert(distill_tools::types::resources::TruncationCfg(cfg));
                }
            }
            (Some(v), None) => {
                resources.insert(distill_tools::types::resources::TruncationCfg(
                    distill_tools::types::context::TruncationConfig {
                        mcp_max_output_bytes: Some(v),
                        ..Default::default()
                    },
                ));
            }
            (None, None) => {}
        }
    }

    /// Resolve a potentially relative path against the session cwd.
    fn resolve_path(cwd: &str, path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(path);
        if p.is_relative() {
            std::path::Path::new(cwd).join(p)
        } else {
            p.to_path_buf()
        }
    }

    /// Whether `name` resolves to a managed-policy (non-disableable) hook in the live registry, keyed on the spec's typed `layer`, never on the name.
    /// Fails open on a missing registry or name: a disable entry that slips through is inert because the dispatcher re-checks provenance at run time.
    /// This modal check is UX; the dispatcher is the enforcement boundary.
    pub(super) fn is_managed_policy_hook(&self, name: &str) -> bool {
        self.hook_registry
            .borrow()
            .as_ref()
            .is_some_and(|registry| {
                registry
                    .find_by_name(name)
                    .is_some_and(|spec| spec.is_managed_policy())
            })
    }

    // ── Hooks/plugins action handlers (pager modal) ──────────────────

    /// Handle a hooks management action from the pager modal.
    pub(super) async fn handle_hooks_action(
        self: &Arc<Self>,
        action: distill_hooks_plugins_types::HooksAction,
    ) -> distill_hooks_plugins_types::ActionOutcome {
        use distill_hooks_plugins_types::{ActionOutcome, HooksAction, OutcomeStatus};

        match action {
            HooksAction::Reload => {
                let reload_msg = self.reload_hooks_impl().await;
                ActionOutcome {
                    status: OutcomeStatus::Success,
                    message: format!("Hooks reloaded.\n{reload_msg}"),
                    requires_reload: false,
                    requires_restart: false,
                }
            }
            HooksAction::Trust => match Self::do_hooks_trust_project(&self.session_info.cwd) {
                Err(e) => ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: e,
                    requires_reload: false,
                    requires_restart: false,
                },
                Ok(root) => {
                    let reload_msg = self.reload_hooks_impl().await;
                    // Trusting flips the project-config gate: re-seed the repo MCP output cap so it applies without waiting for a config edit
                    self.reseed_mcp_output_cap().await;
                    ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: format!("Trusted: {}.\n{reload_msg}", root.display()),
                        requires_reload: false,
                        requires_restart: false,
                    }
                }
            },
            HooksAction::Untrust => match Self::do_hooks_untrust_project(&self.session_info.cwd) {
                Err(e) => ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: e,
                    requires_reload: false,
                    requires_restart: false,
                },
                Ok((root, false)) => ActionOutcome {
                    status: OutcomeStatus::NotFound,
                    message: format!("Not currently trusted: {}", root.display()),
                    requires_reload: false,
                    requires_restart: false,
                },
                Ok((root, true)) => {
                    let reload_msg = self.reload_hooks_impl().await;
                    // Revoking trust must drop a previously seeded repo MCP output cap right away, not at the next config reload
                    // The resolver is trust-gated, so this call clears it
                    self.reseed_mcp_output_cap().await;
                    ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: format!("Untrusted: {}.\n{reload_msg}", root.display()),
                        requires_reload: false,
                        requires_restart: false,
                    }
                }
            },
            HooksAction::Add { path } => {
                if path.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Path is required.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                // CWE-427: add_hooks_path() validates path is under ~/.grok/.
                match crate::config::add_hooks_path(&path) {
                    Ok(()) => ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: {
                            let reload_msg = self.reload_hooks_impl().await;
                            format!("Added hook path: {path}.\n{reload_msg}")
                        },
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: format!("Failed to add hook path: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            HooksAction::Remove { path } => {
                if path.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Path is required.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                match crate::config::remove_hooks_path(&path) {
                    Ok(true) => ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: {
                            let reload_msg = self.reload_hooks_impl().await;
                            format!("Removed hook path: {path}.\n{reload_msg}")
                        },
                        requires_reload: false,
                        requires_restart: false,
                    },
                    // The message omits the path; the user's selection already identifies the source
                    Ok(false) => ActionOutcome {
                        status: OutcomeStatus::NotFound,
                        message: "Only user-added hook directories can be removed here.".to_owned(),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to remove hook path: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            HooksAction::Disable { hook_name } => {
                // Internal spec names stay out of the user-facing message
                if self.is_managed_policy_hook(&hook_name) {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "This hook is enforced by managed policy and cannot be disabled."
                            .to_owned(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                match distill_hooks::trust::disable_hook(&hook_name) {
                    Ok(()) => {
                        self.refresh_hook_disabled();
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: "Hook disabled.".to_owned(),
                            requires_reload: false,
                            requires_restart: false,
                        }
                    }
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to disable hook: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            HooksAction::Enable { hook_name } => {
                match distill_hooks::trust::enable_hook(&hook_name) {
                    Ok(true) => {
                        self.refresh_hook_disabled();
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: "Hook enabled.".to_owned(),
                            requires_reload: false,
                            requires_restart: false,
                        }
                    }
                    Ok(false) => ActionOutcome {
                        status: OutcomeStatus::NotFound,
                        message: "Hook was not disabled.".to_owned(),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to enable hook: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            HooksAction::ToggleSource {
                hook_names,
                disable,
            } => {
                let mut toggled = 0usize;
                let mut managed_skipped = 0usize;
                for name in &hook_names {
                    // Managed-policy hooks are exempt from bulk disable, same rule as the per-hook Disable action
                    if disable && self.is_managed_policy_hook(name) {
                        managed_skipped += 1;
                        continue;
                    }
                    let ok = if disable {
                        distill_hooks::trust::disable_hook(name).is_ok()
                    } else {
                        // Only an actual removal counts (Ok(false) means it wasn't disabled)
                        distill_hooks::trust::enable_hook(name) == Ok(true)
                    };
                    if ok {
                        toggled += 1;
                    }
                }
                if toggled > 0 {
                    self.refresh_hook_disabled();
                }
                let action = if disable { "Disabled" } else { "Enabled" };
                let mut message = format!("{action} {toggled}/{} hooks", hook_names.len());
                if managed_skipped > 0 {
                    message.push_str(&format!(
                        " ({managed_skipped} enforced by managed policy, not disabled)"
                    ));
                }
                ActionOutcome {
                    status: OutcomeStatus::Success,
                    message,
                    requires_reload: false,
                    requires_restart: false,
                }
            }
        }
    }

    /// Handle a plugins management action from the pager modal.
    pub(super) async fn handle_plugins_action(
        self: &Arc<Self>,
        action: distill_hooks_plugins_types::PluginsAction,
    ) -> distill_hooks_plugins_types::ActionOutcome {
        use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus, PluginsAction};

        match action {
            PluginsAction::Reload => match &self.plugin_registry_handle {
                Some(handle) => {
                    // An explicit user reload forces a full re-copy of local installs
                    let msg = self.reload_plugins_impl(handle, true).await;
                    ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: msg,
                        requires_reload: false,
                        requires_restart: false,
                    }
                }
                None => ActionOutcome {
                    status: OutcomeStatus::Unsupported,
                    message: "No plugin registry handle available.".into(),
                    requires_reload: false,
                    requires_restart: false,
                },
            },
            PluginsAction::Install { source } => {
                if source.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Source is required (git URL or local path).".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                // Shared gated pipeline (parse → registry flock → gate → save → auto-enable): this arm can't
                // bypass the lockdown; blocking work runs off the LocalSet (invariant: plugin/acquire.rs).
                let cwd = self.session_info.cwd.clone();
                let cloned_source = source.clone();
                let installed = tokio::task::spawn_blocking(move || {
                    crate::plugin::install_plugin(&cloned_source, std::path::Path::new(&cwd))
                })
                .await;
                match installed {
                    Ok(Ok(outcome)) => {
                        let count = outcome.plugin_names.len();
                        let mut msg = format!(
                            "Installed {count} plugin(s) from {source}: {}",
                            outcome.plugin_names.join(", ")
                        );
                        for w in &outcome.warnings {
                            msg.push_str(&format!(" (warning: {w})"));
                        }
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: msg,
                            requires_reload: true,
                            requires_restart: false,
                        }
                    }
                    // The policy refusal is a complete, validation-shaped message — match the marketplace-modal
                    // sibling instead of wrapping it as an internal error.
                    Ok(Err(crate::plugin::PluginInstallError::Blocked { reason })) => {
                        ActionOutcome {
                            status: OutcomeStatus::ValidationError,
                            message: reason,
                            requires_reload: false,
                            requires_restart: false,
                        }
                    }
                    Ok(Err(e)) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to install plugin: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Install task failed: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Uninstall {
                plugin_id,
                confirmed,
            } => {
                if plugin_id.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Plugin ID is required.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                // Shared uninstall keeps this arm aligned with the CLI path; registry + fs work runs on the
                // blocking pool (invariant: plugin/acquire.rs).
                let task = tokio::task::spawn_blocking(move || {
                    // Extract plugin name from ID (last segment of "scope/hex8/name").
                    let plugin_name = plugin_id.rsplit('/').next().unwrap_or(&plugin_id);
                    crate::plugin::uninstall_plugin(plugin_name, confirmed, false)
                })
                .await;
                use crate::plugin::UninstallError;
                match task {
                    Ok(Ok(outcome)) => ActionOutcome {
                        status: OutcomeStatus::Success,
                        message: format!(
                            "Uninstalled repo \"{}\" ({} plugin(s): {})",
                            outcome.repo_key,
                            outcome.removed_plugins.len(),
                            outcome.removed_plugins.join(", ")
                        ),
                        requires_reload: true,
                        requires_restart: false,
                    },
                    Ok(Err(UninstallError::NeedsConfirm {
                        name,
                        repo_key,
                        other_plugins,
                        total,
                    })) => ActionOutcome {
                        status: OutcomeStatus::ConfirmationRequired,
                        message: format!(
                            "Repo \"{repo_key}\" contains {total} plugin(s): {}. Uninstalling will remove all of them.",
                            std::iter::once(name)
                                .chain(other_plugins)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Err(UninstallError::NotFound { name })) => ActionOutcome {
                        status: OutcomeStatus::NotFound,
                        message: format!("Plugin \"{name}\" not found in install registry."),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Err(UninstallError::RegistryLock { detail })) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Another plugin operation is in progress: {detail}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Err(e @ UninstallError::RegistrySave { .. })) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: e.to_string(),
                        // Files are gone even though the registry is stale.
                        requires_reload: true,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Uninstall task failed: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Update { plugin_id } => {
                use crate::plugin::{RepoUpdateOutcome, UpdateError};

                // Shared gated update pipeline: this arm can't force-sync a blocked source; sync fetches
                // run off the LocalSet (invariant: plugin/acquire.rs).
                let name = plugin_id
                    .as_deref()
                    .map(|id| id.rsplit('/').next().unwrap_or(id).to_string());
                let task = tokio::task::spawn_blocking(move || {
                    crate::plugin::update_plugins(name.as_deref())
                })
                .await;
                match task {
                    Ok(Ok(outcomes)) if outcomes.is_empty() => ActionOutcome {
                        status: OutcomeStatus::NotFound,
                        message: "No installed plugins to update.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Ok(outcomes)) => {
                        let any_updated = outcomes
                            .iter()
                            .any(crate::plugin::repo_update_requires_reload);
                        let messages: Vec<String> = outcomes
                            .iter()
                            .map(|o| match o {
                                RepoUpdateOutcome::Updated { repo_key, .. } => {
                                    format!("{repo_key}: updated")
                                }
                                RepoUpdateOutcome::AlreadyUpToDate { repo_key } => {
                                    format!("{repo_key}: already up to date")
                                }
                                RepoUpdateOutcome::Pinned { repo_key, ref_name } => {
                                    format!("{repo_key}: pinned to {ref_name}")
                                }
                                RepoUpdateOutcome::LiveLocal { repo_key } => {
                                    format!("{repo_key}: local symlink (already live)")
                                }
                                RepoUpdateOutcome::Failed { repo_key, error } => {
                                    format!("{repo_key}: update failed: {error}")
                                }
                            })
                            .collect();
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: messages.join("\n"),
                            requires_reload: any_updated,
                            requires_restart: false,
                        }
                    }
                    Ok(Err(UpdateError::NotFound { name })) => ActionOutcome {
                        status: OutcomeStatus::NotFound,
                        message: format!("Plugin \"{name}\" not found."),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Err(UpdateError::RegistryLock { detail })) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Another plugin operation is in progress: {detail}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                    Ok(Err(e @ UpdateError::RegistrySave { .. })) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: e.to_string(),
                        // Fetched updates may be live on disk despite the
                        // stale registry.
                        requires_reload: true,
                        requires_restart: false,
                    },
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Update task failed: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Add { path } => {
                if path.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Path is required.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                let resolved = Self::resolve_path(&self.session_info.cwd, &path);
                let path_str = resolved.display().to_string();
                match crate::config::run_add_plugin_path(path_str.clone()).await {
                    Ok(()) => {
                        let mut msg = format!("Added plugin path: {path_str}");
                        if let Some(ref handle) = self.plugin_registry_handle {
                            let reload_msg = self.reload_plugins_impl(handle, false).await;
                            msg.push('\n');
                            msg.push_str(&reload_msg);
                        }
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: msg,
                            requires_reload: false,
                            requires_restart: false,
                        }
                    }
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to add plugin path: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Enable { plugin_id } => {
                match crate::config::run_set_plugin_enabled(plugin_id.clone(), true).await {
                    Ok(()) => {
                        if let Some(ref handle) = self.plugin_registry_handle {
                            let reload_msg = self.reload_plugins_impl(handle, false).await;
                            ActionOutcome {
                                status: OutcomeStatus::Success,
                                message: format!("Enabled: {plugin_id}.\n{reload_msg}"),
                                requires_reload: false,
                                requires_restart: false,
                            }
                        } else {
                            ActionOutcome {
                                status: OutcomeStatus::Success,
                                message: format!("Enabled: {plugin_id}. Restart to apply."),
                                requires_reload: false,
                                requires_restart: true,
                            }
                        }
                    }
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to enable plugin: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Disable { plugin_id } => {
                match crate::config::run_set_plugin_enabled(plugin_id.clone(), false).await {
                    Ok(()) => {
                        if let Some(ref handle) = self.plugin_registry_handle {
                            let reload_msg = self.reload_plugins_impl(handle, false).await;
                            ActionOutcome {
                                status: OutcomeStatus::Success,
                                message: format!("Disabled: {plugin_id}.\n{reload_msg}"),
                                requires_reload: false,
                                requires_restart: false,
                            }
                        } else {
                            ActionOutcome {
                                status: OutcomeStatus::Success,
                                message: format!("Disabled: {plugin_id}. Restart to apply."),
                                requires_reload: false,
                                requires_restart: true,
                            }
                        }
                    }
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to disable plugin: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
            PluginsAction::Remove { path } => {
                if path.is_empty() {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: "Path is required.".into(),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
                let resolved = Self::resolve_path(&self.session_info.cwd, &path);
                let path_str = resolved.display().to_string();
                match crate::config::run_remove_plugin_path(path_str.clone()).await {
                    Ok(()) => {
                        let mut msg = format!("Removed plugin path: {path_str}");
                        if let Some(ref handle) = self.plugin_registry_handle {
                            let reload_msg = self.reload_plugins_impl(handle, false).await;
                            msg.push('\n');
                            msg.push_str(&reload_msg);
                        }
                        ActionOutcome {
                            status: OutcomeStatus::Success,
                            message: msg,
                            requires_reload: false,
                            requires_restart: false,
                        }
                    }
                    Err(e) => ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to remove plugin path: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    },
                }
            }
        }
    }

    /// Reload hooks mid-session: re-discovers global and project hooks, re-evaluates project trust, and re-appends plugin-contributed hooks.
    /// `pub(super)` so the `SessionCommand::ReloadHooks` arm in `run_session` (parent module) can call it after an interactive folder-trust grant.
    pub(super) async fn reload_hooks_impl(self: &std::sync::Arc<Self>) -> String {
        let git_root = distill_workspace::session::git::find_git_root_from_path(
            std::path::Path::new(&self.session_info.cwd),
        )
        .ok();
        // Reconcile folder-trust so a mid-session /hooks-trust (or --trust) grant counts on reload, then gate project hook sources on the verdict
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let is_trusted = crate::agent::folder_trust::resolve_and_record(cwd, None, false);
        // discover_hooks is the single load entry point, so all vendors (compat and native) and custom hook-paths match the session-startup sites
        let (mut registry, errors) = crate::util::hooks::discover_hooks(
            git_root.as_deref(),
            &self.rebuild_spec.compat,
            is_trusted,
        );
        for err in &errors {
            tracing::warn!("hook reload error: {err}");
        }
        *self.hook_load_errors.borrow_mut() = errors.iter().map(|e| e.to_string()).collect();
        // Re-append plugin hooks from current plugin registry.
        // Clone the Arc out of the RefCell so the borrow is dropped immediately.
        let plugin_registry_snapshot = self.plugin_registry.borrow().clone();
        if let Some(ref pr) = plugin_registry_snapshot {
            for plugin in pr.active_plugins() {
                if let Some(ref hooks_path) = plugin.hooks_path {
                    let (specs, warnings) =
                        distill_agent::plugins::hooks_adapter::parse_plugin_hooks(
                            hooks_path,
                            &plugin.name,
                            &plugin.root_str(),
                            &plugin.data_dir_str(),
                        );
                    for w in &warnings {
                        tracing::warn!("{w}");
                    }
                    registry.append_specs(specs);
                }
                if let Some(ref inline_value) = plugin.inline_hooks {
                    let (specs, warnings) =
                        distill_agent::plugins::hooks_adapter::parse_plugin_hooks_from_value(
                            inline_value,
                            &plugin.name,
                            &plugin.root_str(),
                            &plugin.data_dir_str(),
                        );
                    for w in &warnings {
                        tracing::warn!("{w}");
                    }
                    registry.append_specs(specs);
                }
            }
        }
        let hook_count = registry.len();
        {
            let mut reg = self.hook_registry.borrow_mut();
            if registry.is_empty() {
                *reg = None;
            } else {
                *reg = Some(std::sync::Arc::new(registry));
            }
        }
        self.refresh_hook_disabled();
        tracing::info!(hook_count, "hooks reloaded mid-session");

        // Notify pager about hooks change.
        // Extract all RefCell borrows into locals before the .await so no Ref guard is alive across the suspension point
        {
            let hooks = crate::extensions::hooks::current_hook_infos(
                self.hook_registry.borrow().as_deref(),
            );
            let load_errors = self.hook_load_errors.borrow().clone();
            let project_trusted = is_trusted;
            self.send_xai_notification(XaiSessionUpdate::HooksChanged {
                hooks,
                project_trusted,
                load_errors,
            })
            .await;
        }
        format!("Hooks reloaded: {hook_count} hook(s) loaded.")
    }

    /// Shared plugin reload logic used by enable/disable/add/remove and the explicit `/plugins reload` command.
    /// `force` is `true` only for the explicit `/plugins reload`, which forces a full re-copy of local installs.
    /// Incidental toggles pass `false` for the cheap skip-unchanged path.
    pub(super) async fn reload_plugins_impl(
        self: &Arc<Self>,
        handle: &distill_agent::plugins::SharedPluginRegistryHandle,
        force: bool,
    ) -> String {
        let session_cwd = std::path::Path::new(&self.session_info.cwd);

        let sid = self.session_info.id.0.as_ref();
        distill_telemetry::unified_log::info("reload_plugins_impl: start", Some(sid), None);

        // Folder-trust gates repo-local project plugins (hooks/MCP).
        // Resolve and record the verdict for this cwd before the plugins-config read below, whose project-paths merge reads the gate.
        // commands/list and the fan-out order these the same way, so no gate read ever precedes the site's own resolve.
        let project_trusted =
            crate::agent::folder_trust::resolve_and_record(session_cwd, None, false);

        let t0 = std::time::Instant::now();
        // Resolve the effective [plugins] config: global, ancestor project configs, and the compat merge
        // Shared with commands/list and the eager fan-out so all paths discover the same plugins for this cwd
        let plugins_cfg = crate::config::resolve_effective_plugins_config(session_cwd);
        let config_read_ms = t0.elapsed().as_millis();

        let t2 = std::time::Instant::now();
        let discovery_config = plugins_cfg.to_discovery_config();
        let count = handle.reload(Some(session_cwd), &discovery_config, project_trusted, force);
        let discover_ms = t2.elapsed().as_millis();

        distill_telemetry::unified_log::info(
            "reload_plugins_impl: discovery done",
            Some(sid),
            Some(serde_json::json!({
                "config_read_ms": config_read_ms as u64,
                "discover_ms": discover_ms as u64,
                "total_ms": t0.elapsed().as_millis() as u64,
                "plugin_count": count,
            })),
        );

        // Adopt plugin hooks, MCP, and plugin-contributed skills into this session.
        // Sessions with `_meta.pluginDirs` rebuild their own view instead; the shared snapshot never carries them
        let session_dirs = self.session_plugin_dirs();
        let new_registry_snapshot = if session_dirs.is_empty() {
            handle.snapshot()
        } else {
            handle.build_for_cwd(
                session_cwd,
                &discovery_config,
                &session_dirs,
                project_trusted,
            )
        };
        let (hooks_reloaded, mcp_changed, skill_count) = self
            .apply_plugin_registry_snapshot(new_registry_snapshot)
            .await;

        let mcp_status = if mcp_changed {
            "MCP refreshed"
        } else {
            "MCP unchanged"
        };
        format!(
            "Plugin registry rebuilt: {count} plugin(s), {hooks_reloaded} hook(s) reloaded, \
             {mcp_status}, {skill_count} skill(s) refreshed."
        )
    }

    /// This session's `_meta.pluginDirs`, recovered from the registry it was built with; empty when the session has none.
    pub(crate) fn session_plugin_dirs(&self) -> Vec<std::path::PathBuf> {
        self.plugin_registry
            .borrow()
            .as_ref()
            .map(|r| r.session_plugin_dirs().to_vec())
            .unwrap_or_default()
    }

    /// Re-merge this session's `_meta.pluginDirs` into a registry rebuilt by a process-wide fan-out (which knows nothing about per-session dirs).
    pub(crate) fn preserve_session_plugin_dirs(
        &self,
        incoming: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    ) -> Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>> {
        let dirs = self.session_plugin_dirs();
        if dirs.is_empty() {
            return incoming;
        }
        let Some(handle) = self.plugin_registry_handle.as_ref() else {
            return incoming;
        };
        let session_cwd = std::path::Path::new(&self.session_info.cwd);
        let disk_cfg =
            crate::config::resolve_effective_plugins_config(session_cwd).to_discovery_config();
        // Reads the stored verdict only; the session's spawn resolve already recorded this cwd with the real remote
        let project_trusted = crate::agent::folder_trust::project_scope_allowed(session_cwd);
        handle.build_for_cwd(session_cwd, &disk_cfg, &dirs, project_trusted)
    }

    /// Apply a pre-built plugin registry snapshot to this session.
    /// Called by `reload_plugins_impl` in the originating session and by the `ReloadPlugins` command when plugins change in another session.
    /// Returns `(hooks_reloaded, mcp_changed, skill_count)`.
    pub(super) async fn apply_plugin_registry_snapshot(
        self: &Arc<Self>,
        new_registry_snapshot: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    ) -> (usize, bool, usize) {
        let sid = self.session_info.id.0.as_ref();
        let session_cwd = std::path::Path::new(&self.session_info.cwd);

        *self.plugin_registry.borrow_mut() = new_registry_snapshot.clone();

        // Reload hooks in the current session
        let t_hooks = std::time::Instant::now();
        let mut hooks_reloaded = 0usize;
        if let Some(ref new_registry) = new_registry_snapshot {
            let mut new_specs = Vec::new();
            for plugin in new_registry.active_plugins() {
                // File-based hooks
                if let Some(ref hooks_path) = plugin.hooks_path {
                    let (specs, warnings) =
                        distill_agent::plugins::hooks_adapter::parse_plugin_hooks(
                            hooks_path,
                            &plugin.name,
                            &plugin.root_str(),
                            &plugin.data_dir_str(),
                        );
                    for w in &warnings {
                        tracing::warn!("{w}");
                    }
                    new_specs.extend(specs);
                }
                // Inline hooks
                if let Some(ref inline_value) = plugin.inline_hooks {
                    let (specs, warnings) =
                        distill_agent::plugins::hooks_adapter::parse_plugin_hooks_from_value(
                            inline_value,
                            &plugin.name,
                            &plugin.root_str(),
                            &plugin.data_dir_str(),
                        );
                    for w in &warnings {
                        tracing::warn!("{w}");
                    }
                    new_specs.extend(specs);
                }
            }
            hooks_reloaded = new_specs.len();
            {
                let mut reg = self.hook_registry.borrow_mut();
                if let Some(ref mut arc_reg) = *reg {
                    let hook_reg = Arc::make_mut(arc_reg);
                    hook_reg.remove_by_prefix("plugin/");
                    hook_reg.append_specs(new_specs);
                } else if !new_specs.is_empty() {
                    // No registry yet: bootstrap config-layer and file hooks the way reload_hooks_impl does
                    // Starting from empty sources instead would let a plugin-first snapshot drop config hooks
                    let git_root =
                        distill_workspace::session::git::find_git_root_from_path(session_cwd).ok();
                    let is_trusted =
                        crate::agent::folder_trust::resolve_and_record(session_cwd, None, false);
                    let (mut new_reg, _errs) = crate::util::hooks::discover_hooks(
                        git_root.as_deref(),
                        &self.rebuild_spec.compat,
                        is_trusted,
                    );
                    new_reg.append_specs(new_specs);
                    *reg = Some(Arc::new(new_reg));
                }
            }
        }

        distill_telemetry::unified_log::info(
            "reload_plugins_impl: hooks done",
            Some(sid),
            Some(serde_json::json!({
                "hooks_reload_ms": t_hooks.elapsed().as_millis() as u64,
                "hooks_reloaded": hooks_reloaded,
            })),
        );

        // Always re-merge plugin-contributed MCP servers and apply them via an order-insensitive diff.
        // Unchanged servers stay connected; only added, changed, or removed ones are re-initialized.
        // The order-sensitive `update_configs` would tear everything down instead, because merge order is non-deterministic.
        let t_mcp = std::time::Instant::now();
        let new_mcp_servers = crate::session::managed_mcp::merge_managed_mcp_servers(
            self.initial_client_mcp_servers.clone(),
            session_cwd,
            new_registry_snapshot.as_deref(),
            &self.rebuild_spec.compat,
        );
        let (mcp_change, dispatch_event_tx) = {
            let mut mcp_state = self.mcp_state.lock().await;
            let change = self.update_mcp_configs(&mut mcp_state, new_mcp_servers);
            let tx = mcp_state.client_event_tx();
            (change, tx)
        };
        let mcp_changed = if let Some(change) = mcp_change {
            self.apply_mcp_config_diff(&change.diff, dispatch_event_tx);
            self.start_mcp_servers_after_config_change(change).await;
            true
        } else {
            false
        };

        distill_telemetry::unified_log::info(
            "reload_plugins_impl: MCP done",
            Some(sid),
            Some(serde_json::json!({
                "mcp_merge_ms": t_mcp.elapsed().as_millis() as u64,
                "mcp_changed": mcp_changed,
            })),
        );

        let t_skills = std::time::Instant::now();
        let bridge = self.agent.borrow().tool_bridge().clone();
        let mut plugin_skills =
            distill_agent::prompt::skills::collect_plugin_skills(new_registry_snapshot.as_deref());
        let disabled = crate::util::config::load_config().await.skills.disabled;
        for skill in &mut plugin_skills {
            if disabled.iter().any(|name| name == &skill.name) {
                skill.enabled = false;
            }
        }
        let skill_count = plugin_skills.len();
        bridge.update_plugin_skills(plugin_skills).await;
        match bridge.apply_pending_skill_update().await {
            Some(effects) => self.apply_skill_update_effects(effects).await,
            None => {
                self.send_available_commands_update(AdvertiseTrigger::SkillsReload)
                    .await
            }
        }
        distill_telemetry::unified_log::info(
            "reload_plugins_impl: skills done",
            Some(sid),
            Some(serde_json::json!({
                "skills_ms": t_skills.elapsed().as_millis() as u64,
                "skill_count": skill_count,
            })),
        );

        // Notify pager about registry changes so the modal auto-refreshes.
        // Extract all RefCell borrows into locals before the .await so no Ref guard is alive across the suspension point
        // Otherwise send_xai_notification's Notification hooks, which also borrow these RefCells, panic with BorrowMutError
        let t_notify = std::time::Instant::now();
        {
            let hooks = crate::extensions::hooks::current_hook_infos(
                self.hook_registry.borrow().as_deref(),
            );
            let load_errors = self.hook_load_errors.borrow().clone();
            // Report the folder-trust verdict so the flag matches the gated registry.
            let project_trusted = crate::agent::folder_trust::project_scope_allowed(
                std::path::Path::new(&self.session_info.cwd),
            );
            self.send_xai_notification(XaiSessionUpdate::HooksChanged {
                hooks,
                project_trusted,
                load_errors,
            })
            .await;

            use crate::extensions::plugins::loaded_plugin_to_info;
            let plugins = {
                let reg = self.plugin_registry.borrow();
                match &*reg {
                    Some(registry) => registry
                        .list()
                        .iter()
                        .map(|p| loaded_plugin_to_info(p))
                        .collect(),
                    None => Vec::new(),
                }
            };
            self.send_xai_notification(XaiSessionUpdate::PluginsChanged { plugins })
                .await;
        }

        distill_telemetry::unified_log::info(
            "apply_plugin_registry_snapshot: complete",
            Some(sid),
            Some(serde_json::json!({
                "notify_ms": t_notify.elapsed().as_millis() as u64,
                "hooks_reloaded": hooks_reloaded,
                "mcp_changed": mcp_changed,
                "skill_count": skill_count,
            })),
        );

        (hooks_reloaded, mcp_changed, skill_count)
    }
}
