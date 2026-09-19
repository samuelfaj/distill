// Modified for Distill by Samuel Fajreldines, 2026.
//! The interactive folder-trust prompt, an ACP round-trip (`x.ai/folder_trust/request`) from the agent to a GUI client (grok-desktop).
//! It asks the client to decide trust for an untrusted workspace that has repo configs.
//! On a grant it reloads the now-trusted project servers without a restart.
//!
//! Dormant in production: it fires only when the connected client advertised `x.ai/folderTrust.interactive`.
//! The folder-trust feature flag must also be on, and the verdict must be [`distill_workspace::folder_trust::TrustOutcome::Prompt`].
//! No client advertises the capability until the desktop UI ships, so this is inert by default even with the feature flag on.
//! The TUI and headless clients never advertise it (they gate trust themselves, client-side), so they are never double-prompted.
//! The module is a child of `mvp_agent` and glob-imports it (`use super::*`).
//!
//! After a grant, MCP, plugins, and each session's own project hooks are hot-reloaded in place.
//! The reload covers every session sharing the granted workspace (same `workspace_key`), each reloaded against its own cwd.
//! Project LSP is not hot-reloaded: the LSP backend is baked into the tool bridge at build time and has no in-place reconfigure API.
//! Repo-local `.grok/lsp.json` servers therefore start on the next session open (the durable grant makes that re-spawn trusted).
//! Project instructions and native project skills apply on the next agent build; trusted plugin skills reconcile in place.
//! `lsp` is still reported in the prompt's `configKinds` because it is a real reason the folder is gated; only the post-grant hot-reload skips it.

use super::*;

/// Max wait for a GUI client's trust decision before giving up (fail-closed); generous because a human is deciding.
/// It bounds the detached task: a client that stays connected without answering (modal left open, a client bug) would otherwise hold it forever.
const TRUST_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// ACP `x.ai/folder_trust/request` payload, sent by the agent to the GUI client.
/// Serialized as `camelCase` for the ACP JSON-RPC wire format.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderTrustRequest {
    /// The session this prompt belongs to.
    /// Required for leader Tier-2 routing: the leader delivers reverse-requests that are not interactions to the driver keyed on `params.sessionId`.
    /// Omitting it makes the leader silently drop the message, so the prompt would never reach the client.
    pub session_id: String,
    /// The session cwd whose workspace is being gated.
    pub cwd: String,
    /// Display path of the canonical workspace key (the trust grant's scope).
    pub workspace: String,
    /// The detected repo-local config kinds (e.g. `mcp`, `hooks`, `lsp`), shown by the prompt UI as the reasons the folder is gated.
    /// Display-only: the trust gate does not read it, though both derive from the same scan.
    /// `lsp` may appear: it is a real reason to prompt, but project LSP applies on the next session open rather than hot-reloading on grant.
    pub config_kinds: Vec<String>,
}

/// Outcome of the trust prompt, sent by the GUI client to the agent.
/// Fail-closed: any value other than `"trust"`, including unknown strings via `#[serde(other)]`, decodes to [`FolderTrustOutcome::Reject`].
/// Only an explicit grant unblocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FolderTrustOutcome {
    Trust,
    #[serde(other)]
    Reject,
}

/// ACP `x.ai/folder_trust/request` response, sent by the GUI client to the agent.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct FolderTrustResponse {
    pub outcome: FolderTrustOutcome,
}

impl MvpAgent {
    /// Parse the `x.ai/folderTrust.interactive` capability from an initialize request.
    /// Returns `false` if absent or not `true`.
    /// Mirrors [`Self::parse_code_nav_capability`].
    pub(crate) fn parse_interactive_trust_capability(init: &acp::InitializeRequest) -> bool {
        init.client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/folderTrust"))
            .and_then(|v| v.get("interactive"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Ask a GUI client to decide trust for `session_id`'s workspace, then grant and reload on accept.
    /// Dormant: it does nothing unless the client advertised `x.ai/folderTrust.interactive` and [`folder_trust::prompt_warranted`] holds.
    /// Non-blocking: the session was already created with project servers gated (the untrusted resolve in `new_session`/`load_session`). Nothing repo-local spawns while the prompt is open. The round-trip and reload run in a detached `spawn_local` task, so the `new_session` response does not wait on the user's decision.
    pub(crate) fn maybe_spawn_interactive_trust_prompt(
        &self,
        session_id: &acp::SessionId,
        cwd: &std::path::Path,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) {
        if !self.interactive_trust_client.get() {
            return;
        }
        if !folder_trust::prompt_warranted(cwd, remote) {
            return;
        }
        let key = distill_workspace::trust::workspace_key(cwd);
        // Dedup: skip if this workspace was already prompted or decided (a reconnect) or has a prompt in flight (a concurrent sibling session)
        // `insert` returns false when already present
        // The set is owned by the agent, not the process, and is captured into the task so failure or timeout can release the key
        let prompted = self.interactive_trust_prompted.clone();
        if !prompted.borrow_mut().insert(key.clone()) {
            return;
        }

        // That matches the per-cwd `handle_reload_project_mcp_servers` and `broadcast_plugin_registry_to_sessions` `&self` can't be borrowed across the `spawn_local` boundary, so capture owned clones now
        // A same-workspace session created while the modal is open is deduped (no second prompt) and absent here, so the grant won't reload it It stays gated until its own next session open, so the miss never over-exposes anything
        // Re-querying at grant time would need the `sessions` map (a non-`Rc` `RefCell` field) shared into the detached task, which we cannot do
        let mut targets = Vec::new();
        self.session_registry.for_each_resident(|_, h| {
            if distill_workspace::trust::workspace_key(std::path::Path::new(&h.info.cwd)) == key {
                targets.push(ReloadTarget {
                    cmd_tx: h.cmd_tx.clone(),
                    initial_client_mcp_servers: h.initial_client_mcp_servers.clone(),
                    cwd: PathBuf::from(&h.info.cwd),
                });
            }
        });
        if targets.is_empty() {
            prompted.borrow_mut().remove(&key);
            return;
        }

        let gateway = self.gateway.clone();
        let plugin_handle = self.plugin_registry_handle.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let remote = remote.cloned();
        let cwd = cwd.to_path_buf();
        let workspace = key.display().to_string();
        let config_kinds = folder_trust::detected_config_kinds(&cwd);
        let session_id = session_id.0.to_string();
        // Regression guard: every reverse-request must carry a non-empty sessionId or leader Tier-2 routing silently drops it
        debug_assert!(
            !session_id.is_empty(),
            "folder_trust reverse-request must carry a non-empty sessionId (design §5.4)"
        );

        tokio::task::spawn_local(async move {
            let request = FolderTrustRequest {
                session_id,
                cwd: cwd.to_string_lossy().into_owned(),
                workspace,
                config_kinds,
            };
            // Non-panicking: a struct of `String` and `Vec<String>` fields can't fail to serialize, but avoid `expect` in prod
            // On the impossible error, bail and release the dedup key rather than aborting the task thread
            let raw_params = match serde_json::value::to_raw_value(&request) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "folder trust: request serialization failed");
                    prompted.borrow_mut().remove(&key);
                    return;
                }
            };
            let ext_request = acp::ExtRequest::new("x.ai/folder_trust/request", raw_params.into());

            use agent_client_protocol::Client as _;
            let outcome = match tokio::time::timeout(
                TRUST_PROMPT_TIMEOUT,
                gateway.ext_method(ext_request),
            )
            .await
            {
                // A decodable response carries the user's decision
                // An undecodable success payload is a client or protocol error, not a decision
                // Stay gated (fail-closed) but release the dedup key so a later session can re-prompt, same as transport and timeout below
                Ok(Ok(raw)) => match serde_json::from_str::<FolderTrustResponse>(raw.0.get()) {
                    Ok(r) => r.outcome,
                    Err(e) => {
                        tracing::debug!(error = %e, "folder trust: undecodable trust response; staying gated, releasing dedup key");
                        prompted.borrow_mut().remove(&key);
                        return;
                    }
                },
                Ok(Err(e)) => {
                    // A disconnect or transport error is not a decision: release the key so a later session can re-prompt
                    tracing::debug!(error = %e, "folder trust: client trust request failed");
                    prompted.borrow_mut().remove(&key);
                    return;
                }
                Err(_elapsed) => {
                    // The client stayed connected but silent past the deadline: stay gated, release the key so a future session may re-prompt
                    tracing::info!(
                        cwd = %cwd.display(),
                        "folder trust: no client decision before timeout; staying gated"
                    );
                    prompted.borrow_mut().remove(&key);
                    return;
                }
            };
            if outcome != FolderTrustOutcome::Trust {
                // The client decided "reject": keep the dedup key so the user is not re-prompted for this workspace on every reconnect
                tracing::info!(
                    cwd = %cwd.display(),
                    "folder trust: GUI client declined; workspace stays gated"
                );
                return;
            }

            // Re-check the dedup key before granting `HooksAction::Untrust` removes this workspace's key (and revokes asynchronously) when the user untrusts
            // If that fired while the modal was open, the key is gone: honor the untrust and drop this now-stale "trust" Re-persisting would restore a grant the user just revoked
            // The single-threaded LocalSet makes this check and the grant atomic with respect to the untrust task (no await in between)
            if !prompted.borrow().contains(&key) {
                tracing::info!(
                    cwd = %cwd.display(),
                    "folder trust: workspace untrusted while prompt was open; ignoring stale grant"
                );
                return;
            }

            // Persist the shown workspace key; do not re-derive workspace_key on accept.
            let outcome = distill_workspace::folder_trust::grant_folder_trust_key(&key);
            if !outcome.dismisses_gate() {
                match &outcome {
                    distill_workspace::folder_trust::GrantOutcome::Granted {
                        persist:
                            distill_workspace::folder_trust::PersistStatus::ProcessLocalOnly { error },
                        ..
                    } => {
                        // Not a durable grant: do not reload or report success.
                        tracing::warn!(
                            cwd = %cwd.display(),
                            error = %error,
                            "folder trust: grant is process-local only and will not survive restart; staying gated"
                        );
                    }
                    distill_workspace::folder_trust::GrantOutcome::Refused { reason } => {
                        // Same as timeout: a refused grant is not a decision, so release the dedup key.
                        tracing::warn!(
                            cwd = %cwd.display(),
                            ?reason,
                            "folder trust: grant refused; staying gated"
                        );
                    }
                    _ => {}
                }
                prompted.borrow_mut().remove(&key);
                return;
            }
            if distill_workspace::trust::workspace_key(&cwd) != key {
                tracing::info!(
                    cwd = %cwd.display(),
                    "folder trust: cwd no longer maps to the granted key; skipping reload"
                );
                return;
            }
            folder_trust::resolve_and_record(&cwd, remote.as_ref(), false);

            reload_project_servers_after_grant(ReloadAfterGrant {
                gateway: &gateway,
                targets,
                plugin_handle: &plugin_handle,
                compat: &compat,
                prompt_cwd: &cwd,
            })
            .await;

            tracing::info!(
                cwd = %cwd.display(),
                "folder trust: granted via GUI client; reloaded project servers"
            );
        });
    }
}

/// One session to reload after a grant, with its own cwd.
/// The MCP merge and plugin build then use the session's own project config (matching the per-cwd canonical reloaders), not the prompt's cwd.
struct ReloadTarget {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    cwd: PathBuf,
}

/// Inputs for [`reload_project_servers_after_grant`], bundled to avoid a long positional arg list.
struct ReloadAfterGrant<'a> {
    gateway: &'a GatewaySender,
    /// Every session sharing the granted workspace, each with its own cwd.
    targets: Vec<ReloadTarget>,
    plugin_handle: &'a distill_agent::plugins::SharedPluginRegistryHandle,
    compat: &'a distill_tools::types::CompatConfig,
    /// The prompting session's cwd, used only for the client catalog push.
    prompt_cwd: &'a std::path::Path,
}

/// Reload each granted-workspace session's now-trusted project servers in place (no restart), against each session's own cwd.
/// Drives the same primitives as the canonical reloaders `handle_reload_project_mcp_servers` and `broadcast_plugin_registry_to_sessions`.
/// LSP is baked in at bridge build and applies on the next session open (see module docs). The caller must have granted and recorded trust first.
async fn reload_project_servers_after_grant(ctx: ReloadAfterGrant<'_>) {
    let plugin_snapshot = ctx.plugin_handle.snapshot();

    for target in ctx.targets {
        // Per-session cwd: a sibling session in a subdir of the granted workspace must get its own project config, not the prompt's
        let session_cwd = target.cwd.as_path();
        // MCP: `merge_managed_mcp_servers` re-reads disk and runs `filter_untrusted_project_mcp`
        // The filter now keeps project servers because the cached verdict was flipped to trusted (same workspace key)
        let _ = crate::session::managed_mcp::merge_and_send_managed_mcp_update(
            &target.cmd_tx,
            session_cwd,
            target.initial_client_mcp_servers,
            plugin_snapshot.as_deref(),
            ctx.compat,
        );
        // Plugins (and plugin-contributed hooks) are built for this session's own cwd on the folder-trust verdict
        // This mirrors `broadcast_plugin_registry_to_sessions`
        // The grant and `resolve_and_record` above flipped the cached verdict to trusted
        let disk_cfg =
            crate::config::resolve_effective_plugins_config(session_cwd).to_discovery_config();
        let project_trusted = folder_trust::project_scope_allowed(session_cwd);
        // Session `_meta.pluginDirs` are re-merged by the receiving actor (`preserve_session_plugin_dirs` on `ReloadPlugins`)
        let registry =
            ctx.plugin_handle
                .build_for_cwd(session_cwd, &disk_cfg, &[], project_trusted);
        let _ = target
            .cmd_tx
            .send(crate::session::SessionCommand::ReloadPlugins { registry });
        // This reloads the session's own project hooks (`.grok/hooks`, `.cursor/hooks.json`), which `ReloadPlugins` does not touch
        // `reload_hooks_impl` re-discovers them against the actor's own `session_info.cwd` on the now-trusted verdict
        let _ = target
            .cmd_tx
            .send(crate::session::SessionCommand::ReloadHooks);
    }

    // Push the refreshed MCP catalog so the client UI reflects the now-trusted repo-local servers;
    // policy-blocked servers stay out, same as the pool seeding.
    let local = folder_trust::filter_untrusted_project_mcp(
        ctx.prompt_cwd,
        crate::util::config::load_mcp_servers(ctx.prompt_cwd, ctx.compat),
    );
    let local = crate::session::managed_mcp::filter_policy_blocked_agent_mcp(local, ctx.prompt_cwd);
    crate::extensions::mcp::notify_servers_updated(ctx.gateway, &local).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_with_meta(meta: Option<serde_json::Value>) -> acp::InitializeRequest {
        // Production reads `client_capabilities.meta`, not top-level request meta.
        let mut caps = acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false);
        if let Some(m) = meta
            && let Some(map) = m.as_object().cloned()
        {
            caps = caps.meta(map);
        }
        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(caps)
    }

    #[test]
    fn parse_interactive_trust_capability_present_and_true() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "x.ai/folderTrust".to_string(),
            serde_json::json!({ "interactive": true }),
        );
        let init = init_with_meta(Some(serde_json::Value::Object(meta)));
        assert!(MvpAgent::parse_interactive_trust_capability(&init));
    }

    #[test]
    fn parse_interactive_trust_capability_absent_returns_false() {
        let init = init_with_meta(None);
        assert!(!MvpAgent::parse_interactive_trust_capability(&init));
    }

    #[test]
    fn parse_interactive_trust_capability_false_returns_false() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "x.ai/folderTrust".to_string(),
            serde_json::json!({ "interactive": false }),
        );
        let init = init_with_meta(Some(serde_json::Value::Object(meta)));
        assert!(!MvpAgent::parse_interactive_trust_capability(&init));
    }

    #[test]
    fn request_serializes_camel_case_with_session_id() {
        let req = FolderTrustRequest {
            session_id: "sess-1".into(),
            cwd: "/repo".into(),
            workspace: "/repo".into(),
            config_kinds: vec!["mcp".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("configKinds").is_some());
        assert!(json.get("config_kinds").is_none());
        // Leader Tier-2 routing reads `params.sessionId`; it must be present and non-empty
        // Regression guard: without it the leader silently drops the request
        let Some(session_id) = json.get("sessionId").and_then(|v| v.as_str()) else {
            panic!("sessionId missing: {json:?}");
        };
        assert_eq!(session_id, "sess-1");
        assert!(!session_id.is_empty());
    }

    #[test]
    fn response_decodes_trust_reject_and_unknown_fail_closed() {
        let trust: FolderTrustResponse = serde_json::from_str(r#"{"outcome":"trust"}"#).unwrap();
        assert_eq!(trust.outcome, FolderTrustOutcome::Trust);
        let reject: FolderTrustResponse = serde_json::from_str(r#"{"outcome":"reject"}"#).unwrap();
        assert_eq!(reject.outcome, FolderTrustOutcome::Reject);
        // Unknown outcome must fail closed to Reject (never silently "trust").
        let unknown: FolderTrustResponse = serde_json::from_str(r#"{"outcome":"banana"}"#).unwrap();
        assert_eq!(unknown.outcome, FolderTrustOutcome::Reject);
    }
}
