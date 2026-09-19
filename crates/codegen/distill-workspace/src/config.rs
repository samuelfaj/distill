// Modified for Distill by Samuel Fajreldines, 2026.
//! Workspace and session configuration types.
use crate::capability::CapabilityMode;
use crate::hub::HubConfig;
use crate::permission::ToolApprovalGate;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use distill_hooks::discovery::HookSource;
use distill_tools::registry::types::{SessionContext, ToolRegistryBuilder, ToolServerConfig};
use distill_tool_runtime::ToolApprovalPolicy;
/// Default capacity for the workspace event broadcast channel.
pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 64;
/// A session-lifetime terminal backend (background-task registry and persistent shell) paired with its explicit shutdown hook.
/// Teardown at `drop_session`/evict is an explicit act rather than a side effect of the last `Arc` drop.
#[derive(Clone)]
pub struct SessionTerminalBackend {
    backend: Arc<dyn distill_tools::computer::types::TerminalBackend>,
    shutdown: Arc<dyn Fn() + Send + Sync>,
}
impl SessionTerminalBackend {
    /// Pair an already-erased `backend` with its shutdown hook. The fields are private, so [`SessionContextFactory`] implementors whose backend is not a `LocalTerminalBackend` must build one here.
    pub fn new(
        backend: Arc<dyn distill_tools::computer::types::TerminalBackend>,
        shutdown: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self { backend, shutdown }
    }
    /// Wrap a [`LocalTerminalBackend`], wiring the shutdown hook to its cancel token.
    ///
    /// [`LocalTerminalBackend`]: distill_tools::computer::local::LocalTerminalBackend
    pub fn local(backend: distill_tools::computer::local::LocalTerminalBackend) -> Self {
        let canceller = backend.clone();
        Self {
            backend: Arc::new(backend),
            shutdown: Arc::new(move || canceller.cancel()),
        }
    }
    /// The type-erased backend, as injected into toolset resolves.
    pub fn backend(&self) -> &Arc<dyn distill_tools::computer::types::TerminalBackend> {
        &self.backend
    }
    /// Explicitly shut the backend down: kills all of its child process groups and stops its actor.
    pub fn shutdown(&self) {
        (self.shutdown)();
    }
}
impl std::fmt::Debug for SessionTerminalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTerminalBackend")
            .finish_non_exhaustive()
    }
}
/// Pluggable producer of a [`SessionContext`] and [`ToolRegistryBuilder`] for each session. The workspace can't construct the tool runtime (terminal backend, file system, persistence path, MCP client config, notification handle, ...).
pub trait SessionContextFactory: Send + Sync {
    /// Build a fresh [`SessionContext`] for the given session, around the given terminal `backend`.
    /// The pipeline rebuilds toolsets around the session-owned backend, so the caller always supplies it.
    /// Constructing a backend here would waste an actor per resolve.
    fn build_session_context(
        &self,
        session_id: &str,
        cwd: PathBuf,
        session_env: Arc<HashMap<String, String>>,
        backend: Arc<dyn distill_tools::computer::types::TerminalBackend>,
    ) -> SessionContext;
    /// Build the session-lifetime terminal backend for a new session.
    /// Called once per session create/fork; toolset re-resolves reuse the session's stored backend instead of building another.
    fn build_terminal_backend(&self) -> SessionTerminalBackend;
    /// Build a fresh [`ToolRegistryBuilder`] with the workspace's full set of registered tools.
    fn registry_builder(&self) -> ToolRegistryBuilder;
    fn known_tool_ids(&self) -> Arc<std::collections::HashSet<String>> {
        Arc::new(self.registry_builder().known_tool_ids())
    }
}
/// Placeholder for the cross-session memory backend config.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MemoryConfig {}
/// Per-session toolset/capability selection from the `session.bind` metadata.
/// Absent fields fall back to the workspace default and `CapabilityMode::All`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct WorkspaceBindConfig {
    /// Named toolset preset from the wire. **Never resolved** (see [`Self::resolve`]); parsed only so it can be logged.
    pub preset: Option<String>,
    /// Capability mode applied to the session's toolset.
    pub capability_mode: Option<CapabilityMode>,
    /// Fully-specified toolset in the runtime serde shape. Takes precedence over `tools`.
    pub tool_config: Option<ToolServerConfig>,
    /// Per-user feature-flag bag. `None` on legacy payloads means tools fall back to their safe defaults.
    pub viewer_ctx: Option<distill_tool_runtime::WorkspaceViewerContext>,
    /// Initial auto-approve (YOLO) state. `None` on legacy payloads fails closed (false).
    pub yolo_mode: Option<bool>,
    /// The hub-set attended-execution ceiling; omitted → [`ToolApprovalPolicy::GrantsAllowed`],
    /// present-but-unparseable → [`ToolApprovalPolicy::AlwaysPrompt`].
    pub tool_approval_policy: ToolApprovalPolicy,
    /// Plane-configured toolset in the gRPC wire shape. An empty list is treated as unset (proto3 repeated default).
    pub tools: Option<Vec<distill_tools_api::ToolConfigEntry>>,
    pub manifest_version: Option<String>,
    pub manifest_hash: Option<String>,
    /// Opt-in: forward `BackgroundTaskCompleted` system notifications for this session.
    pub system_notifications: bool,
    pub rpc_only: bool,
    /// Real guest session root (`/workspace/<conversation_id>`). When set, the workspace virtualizes that tree as `/workspace`.
    pub session_root: Option<PathBuf>,
}
/// Outcome of resolving a [`WorkspaceBindConfig`]; lets callers fail closed instead of widening to the default toolset.
/// Deliberately has **no preset arm** (see [`WorkspaceBindConfig::resolve`]).
#[derive(Debug)]
pub enum ResolvedToolset {
    /// An explicit toolset (`tool_config` or `tools`).
    Toolset(ResolvedTools),
    /// No explicit toolset was specified and the workspace allows falling back to its default catalog (local/CLI embedders only).
    UseDefault,
    /// No explicit toolset was specified and the workspace requires one (sandbox-launched standalone servers): fail closed.
    MissingToolConfig,
    /// `tools` entries were specified but at least one failed to convert.
    InvalidToolConfig(distill_tools::registry::proto_convert::ToolConfigEntryError),
}
/// A resolved toolset plus the pinned entries this binary could not serve.
#[derive(Debug)]
pub struct ResolvedTools {
    pub toolset: ToolServerConfig,
    /// Pinned `tools` ids unknown to this binary's registry, sorted.
    /// Always empty for `tool_config` resolutions.
    pub unserved_tool_ids: Vec<String>,
}
impl ResolvedTools {
    fn full(toolset: ToolServerConfig) -> Self {
        Self {
            toolset,
            unserved_tool_ids: Vec::new(),
        }
    }
}
impl WorkspaceBindConfig {
    /// Parse hub `session.bind` metadata.
    /// The envelope is the shared [`distill_tool_runtime::WorkspaceBindMetadata`], the same type the emitter serializes.
    /// `tool_config` is a consumer-only raw escape hatch read separately.
    pub fn from_metadata(metadata: &serde_json::Value) -> Self {
        let wire: distill_tool_runtime::WorkspaceBindMetadata =
            serde_json::from_value(metadata.clone()).unwrap_or_default();
        Self {
            preset: wire.preset,
            capability_mode: wire
                .capability_mode
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok()),
            tool_config: metadata
                .as_object()
                .and_then(|obj| obj.get("tool_config"))
                .and_then(|v| parse_field("tool_config", v)),
            viewer_ctx: wire.viewer_ctx,
            yolo_mode: wire.yolo_mode,
            tool_approval_policy: match wire.tool_approval_policy {
                Some(Ok(policy)) => policy,
                Some(Err(raw)) => {
                    tracing::warn!(
                        tool_approval_policy = %raw,
                        "session.bind metadata: unreadable tool_approval_policy; applying always_prompt"
                    );
                    ToolApprovalPolicy::AlwaysPrompt
                }
                None => ToolApprovalPolicy::GrantsAllowed,
            },
            tools: Some(wire.tools).filter(|tools| !tools.is_empty()),
            manifest_version: wire.manifest_version,
            manifest_hash: wire.manifest_hash,
            system_notifications: wire.system_notifications.unwrap_or(false),
            rpc_only: wire.rpc_only,
            session_root: wire.session_root.map(PathBuf::from),
        }
    }
    /// Resolve the selected toolset. Precedence: `tool_config`, then `tools` (wire entries), then default/fail-closed.
    /// Unknown ids never cause a silent fallback to a different toolset.
    pub fn resolve(
        &self,
        known_id: &dyn Fn(&str) -> bool,
        require_explicit_toolset: bool,
    ) -> ResolvedToolset {
        if let Some(cfg) = &self.tool_config {
            for (idx, tool) in cfg.tools.iter().enumerate() {
                if let Err(err) = distill_tools_api::config_validation::validate_name_override(
                    idx,
                    &tool.id,
                    tool.name_override.as_deref(),
                ) {
                    return ResolvedToolset::InvalidToolConfig(err);
                }
            }
            return ResolvedToolset::Toolset(ResolvedTools::full(cfg.clone()));
        }
        if let Some(tools) = &self.tools {
            let mut unserved_tool_ids: Vec<String> = Vec::new();
            let mut served = Vec::with_capacity(tools.len());
            for (idx, entry) in tools.iter().enumerate() {
                if !known_id(&entry.id) {
                    unserved_tool_ids.push(entry.id.clone());
                    continue;
                }
                match distill_tools::registry::proto_convert::tool_config_from_entry(
                    idx,
                    entry.clone(),
                ) {
                    Ok(tc) => served.push(tc),
                    Err(err) => return ResolvedToolset::InvalidToolConfig(err),
                }
            }
            unserved_tool_ids.sort_unstable();
            if !unserved_tool_ids.is_empty() {
                tracing::warn!(
                    unserved = ?unserved_tool_ids,
                    config_manifest_version = ?self.manifest_version,
                    running_version = distill_version::VERSION,
                    "session.bind: serving known subset of pinned tools"
                );
            }
            return ResolvedToolset::Toolset(ResolvedTools {
                toolset: ToolServerConfig {
                    tools: served,
                    behavior_preset: None,
                },
                unserved_tool_ids,
            });
        }
        if let Some(preset) = self.preset.as_deref() {
            tracing::warn!(
                preset,
                "session.bind: toolset presets are not resolved by the workspace \
                 server; pass an explicit `tools` config"
            );
        }
        if require_explicit_toolset {
            ResolvedToolset::MissingToolConfig
        } else {
            ResolvedToolset::UseDefault
        }
    }
}
/// Parse a single bind-metadata field, ignoring (and logging) a malformed value.
fn parse_field<T: serde::de::DeserializeOwned>(name: &str, value: &serde_json::Value) -> Option<T> {
    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            tracing::warn!(
                field = name,
                error = %e,
                "session.bind metadata: ignoring malformed field"
            );
            None
        }
    }
}
#[cfg(test)]
mod bind_config_tests {
    use super::*;
    /// Predicate for tests where every pinned id is known to the binary.
    fn all_known(_: &str) -> bool {
        true
    }
    /// Predicate for tests simulating a binary that knows none of the ids.
    fn none_known(_: &str) -> bool {
        false
    }
    #[test]
    fn parses_preset_and_capability() {
        let v = serde_json::json!({"preset": "explore", "capability_mode": "read_only"});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert_eq!(cfg.preset.as_deref(), Some("explore"));
        assert_eq!(
            cfg.capability_mode,
            Some(crate::capability::CapabilityMode::ReadOnly)
        );
    }
    #[test]
    fn defaults_on_empty_or_mismatched_metadata() {
        let empty = WorkspaceBindConfig::from_metadata(&serde_json::json!({}));
        assert!(empty.preset.is_none());
        assert!(empty.capability_mode.is_none());
        assert!(matches!(
            empty.resolve(&all_known, false),
            ResolvedToolset::UseDefault
        ));
        let weird = WorkspaceBindConfig::from_metadata(&serde_json::json!("hello"));
        assert!(matches!(
            weird.resolve(&all_known, false),
            ResolvedToolset::UseDefault
        ));
    }
    /// Presets are banned: any preset (known or not) is ignored, never resolved to a toolset, and never widened to the default in strict mode.
    #[test]
    fn presets_are_never_resolved() {
        for preset in ["explore", "grok-computer", "bogus"] {
            let cfg = WorkspaceBindConfig::from_metadata(&serde_json::json!({ "preset": preset }));
            assert!(
                matches!(cfg.resolve(&all_known, false), ResolvedToolset::UseDefault),
                "lax mode must fall through to the default, preset={preset}"
            );
            assert!(
                matches!(
                    cfg.resolve(&all_known, true),
                    ResolvedToolset::MissingToolConfig
                ),
                "strict mode must fail closed, preset={preset}"
            );
        }
    }
    /// Strict mode (sandbox standalone server): a bind without an explicit toolset fails closed instead of widening to the default catalog.
    #[test]
    fn strict_mode_requires_explicit_toolset() {
        let empty = WorkspaceBindConfig::from_metadata(&serde_json::json!({}));
        assert!(matches!(
            empty.resolve(&all_known, true),
            ResolvedToolset::MissingToolConfig
        ));
    }
    #[test]
    fn malformed_field_does_not_discard_valid_siblings() {
        let v = serde_json::json!({"preset": "explore", "capability_mode": "raed_only"});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert_eq!(cfg.preset.as_deref(), Some("explore"));
        assert!(cfg.capability_mode.is_none());
    }
    #[test]
    fn workspace_bind_config_from_metadata_extracts_viewer_ctx() {
        let v = serde_json::json!({
            "preset": "explore",
            "viewer_ctx": {"stream_tool_progress": true},
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert_eq!(cfg.preset.as_deref(), Some("explore"));
        let viewer = cfg.viewer_ctx.expect("viewer_ctx parsed");
        assert!(viewer.stream_tool_progress);
    }
    /// Legacy payload without `viewer_ctx` still parses (mixed-version proxy/workspace deploys).
    #[test]
    fn workspace_bind_config_from_metadata_legacy_omitted_viewer_ctx() {
        let v = serde_json::json!({"preset": "explore"});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert!(cfg.viewer_ctx.is_none());
    }
    #[test]
    fn workspace_bind_config_from_metadata_extracts_yolo_mode() {
        let v = serde_json::json!({"preset": "explore", "yolo_mode": true});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert_eq!(cfg.yolo_mode, Some(true));
    }
    #[test]
    fn workspace_bind_config_yolo_mode_omitted_or_malformed_fails_closed() {
        let omitted = WorkspaceBindConfig::from_metadata(&serde_json::json!({"preset": "explore"}));
        assert!(omitted.yolo_mode.is_none());
        let malformed = WorkspaceBindConfig::from_metadata(
            &serde_json::json!({"preset": "explore", "yolo_mode": "yes"}),
        );
        assert!(malformed.yolo_mode.is_none());
        assert_eq!(malformed.preset.as_deref(), Some("explore"));
    }
    #[test]
    fn workspace_bind_config_tool_approval_policy_defaults_to_grants_allowed() {
        let ceiling = WorkspaceBindConfig::from_metadata(
            &serde_json::json!({"tool_approval_policy": "always_prompt"}),
        );
        assert_eq!(
            ceiling.tool_approval_policy,
            ToolApprovalPolicy::AlwaysPrompt
        );
        for metadata in [
            serde_json::json!({"preset": "explore"}),
            serde_json::json!({"tool_approval_policy": null}),
        ] {
            assert_eq!(
                ToolApprovalPolicy::GrantsAllowed,
                WorkspaceBindConfig::from_metadata(&metadata).tool_approval_policy,
                "{metadata}"
            );
        }
        assert_eq!(
            ToolApprovalPolicy::UnattendedAllowed,
            WorkspaceBindConfig::from_metadata(&serde_json::json!({
                "tool_approval_policy": "unattended_allowed",
            }))
            .tool_approval_policy,
        );
    }
    /// A ceiling the hub stamped but this binary cannot read (a newer wire token, a typo,
    /// the wrong JSON type) is the strict arm, never the omitted-field compat default.
    #[test]
    fn workspace_bind_config_malformed_tool_approval_policy_is_always_prompt() {
        for metadata in [
            serde_json::json!({"tool_approval_policy": "unattended"}),
            serde_json::json!({"tool_approval_policy": "not_a_policy"}),
            serde_json::json!({"tool_approval_policy": "GrantsAllowed"}),
            serde_json::json!({"tool_approval_policy": true}),
            serde_json::json!({"tool_approval_policy": 1}),
            serde_json::json!({"preset": "explore", "tool_approval_policy": ["grants_allowed"]}),
        ] {
            let cfg = WorkspaceBindConfig::from_metadata(&metadata);
            assert_eq!(
                ToolApprovalPolicy::AlwaysPrompt,
                cfg.tool_approval_policy,
                "{metadata}"
            );
        }
        let cfg = WorkspaceBindConfig::from_metadata(
            &serde_json::json!({"preset": "explore", "tool_approval_policy": "unattended"}),
        );
        assert_eq!(Some("explore"), cfg.preset.as_deref());
    }
    #[test]
    fn workspace_bind_config_extracts_system_notifications_flag() {
        let on =
            WorkspaceBindConfig::from_metadata(&serde_json::json!({"system_notifications": true}));
        assert!(on.system_notifications);
        let off = WorkspaceBindConfig::from_metadata(&serde_json::json!({"preset": "explore"}));
        assert!(!off.system_notifications);
        let explicit_off =
            WorkspaceBindConfig::from_metadata(&serde_json::json!({"system_notifications": false}));
        assert!(!explicit_off.system_notifications);
    }
    #[test]
    fn workspace_bind_config_extracts_rpc_only_flag() {
        let on = WorkspaceBindConfig::from_metadata(&serde_json::json!({"rpc_only": true}));
        assert!(on.rpc_only);
        let off = WorkspaceBindConfig::from_metadata(&serde_json::json!({"preset": "explore"}));
        assert!(!off.rpc_only);
        let explicit_off =
            WorkspaceBindConfig::from_metadata(&serde_json::json!({"rpc_only": false}));
        assert!(!explicit_off.rpc_only);
    }
    #[test]
    fn workspace_bind_config_extracts_session_root() {
        let on = WorkspaceBindConfig::from_metadata(&serde_json::json!({
            "session_root": "/workspace/conv-abc",
        }));
        assert_eq!(
            on.session_root.as_deref(),
            Some(std::path::Path::new("/workspace/conv-abc"))
        );
        let off = WorkspaceBindConfig::from_metadata(&serde_json::json!({"preset": "explore"}));
        assert!(off.session_root.is_none());
        let malformed = WorkspaceBindConfig::from_metadata(&serde_json::json!({
            "session_root": 123,
            "preset": "explore",
        }));
        assert!(malformed.session_root.is_none());
        assert_eq!(malformed.preset.as_deref(), Some("explore"));
    }
    #[test]
    fn workspace_bind_config_from_metadata_extracts_manifest_fields() {
        let v = serde_json::json!({
            "preset": "explore",
            "manifest_version": "v1",
            "manifest_hash": "abc123",
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert_eq!(cfg.manifest_version.as_deref(), Some("v1"));
        assert_eq!(cfg.manifest_hash.as_deref(), Some("abc123"));
    }
    #[test]
    fn workspace_bind_config_manifest_fields_default_to_none_when_absent() {
        let cfg = WorkspaceBindConfig::from_metadata(&serde_json::json!({"preset": "explore"}));
        assert!(cfg.manifest_version.is_none());
        assert!(cfg.manifest_hash.is_none());
    }
    /// Consumer-side parity test for the bind-metadata `tools` contract.
    /// Pairs with the producer-side pin test in agentic-sampler's `configs::plane` tests.
    #[test]
    fn tools_entries_resolve_to_tool_server_config() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [
                {
                    "id": "Distill:grep",
                    "params_json": "{\"max_results\":50}",
                    "name_override": "search",
                    "params_name_overrides": {"pattern": "query"},
                    "behavior_version": "legacy-0.4.10",
                    "description_override": "Search the codebase",
                },
                {"id": "Distill:read_file"},
            ],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, false) else {
            panic!("tools entries must resolve to an explicit toolset");
        };
        assert!(resolved.unserved_tool_ids.is_empty());
        let toolset = resolved.toolset;
        assert_eq!(
            toolset.behavior_preset, None,
            "always the 'current' default"
        );
        assert_eq!(toolset.tools.len(), 2);
        let Some(grep) = toolset.tools.first() else {
            panic!("expected grep tool: {:?}", toolset.tools);
        };
        assert_eq!(grep.id, "Distill:grep");
        assert_eq!(
            grep.params,
            serde_json::json!({"max_results": 50}).as_object().cloned()
        );
        assert_eq!(grep.name_override.as_deref(), Some("search"));
        assert_eq!(
            grep.params_name_overrides
                .as_ref()
                .and_then(|m| m.get("pattern"))
                .map(String::as_str),
            Some("query")
        );
        assert_eq!(grep.behavior_version.as_deref(), Some("legacy-0.4.10"));
        assert_eq!(
            grep.description_override.as_deref(),
            Some("Search the codebase")
        );
        assert_eq!(grep.kind, None);
        assert_eq!(
            toolset.tools.get(1).map(|t| t.id.as_str()),
            Some("Distill:read_file")
        );
    }
    #[test]
    fn explicit_tool_config_wins_over_tools_entries() {
        let v = serde_json::json!({
            "tool_config": {"tools": [{"id": "raw:tool"}]},
            "tools": [{"id": "wire:tool"}],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, false) else {
            panic!("must resolve to a toolset");
        };
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("raw:tool")
        );
    }
    #[test]
    fn tools_entries_win_even_with_preset_present() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [{"id": "wire:tool"}],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, false) else {
            panic!("must resolve to a toolset");
        };
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("wire:tool")
        );
    }
    #[test]
    fn empty_tools_array_is_treated_as_unset() {
        let v = serde_json::json!({"preset": "explore", "tools": []});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert!(cfg.tools.is_none());
        assert!(matches!(
            cfg.resolve(&all_known, false),
            ResolvedToolset::UseDefault
        ));
        assert!(matches!(
            cfg.resolve(&all_known, true),
            ResolvedToolset::MissingToolConfig
        ));
        let no_preset = serde_json::json!({"tools": []});
        let cfg = WorkspaceBindConfig::from_metadata(&no_preset);
        assert!(matches!(
            cfg.resolve(&all_known, false),
            ResolvedToolset::UseDefault
        ));
    }
    #[test]
    fn invalid_tools_entry_fails_closed() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [{"id": "bad:tool", "params_json": "{not json"}],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        match cfg.resolve(&all_known, false) {
            ResolvedToolset::InvalidToolConfig(err) => {
                assert_eq!(err.tool_id, "bad:tool");
                assert_eq!(err.index, 0);
            }
            other => panic!("expected InvalidToolConfig, got {other:?}"),
        }
    }
    #[test]
    fn invalid_name_override_fails_closed() {
        let v = serde_json::json!({
            "tools": [
                {"id": "wire:ok", "name_override": "fine_name"},
                {"id": "wire:bad", "name_override": "not a tool id!"},
            ],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        match cfg.resolve(&all_known, true) {
            ResolvedToolset::InvalidToolConfig(err) => {
                assert_eq!(err.tool_id, "wire:bad");
                assert_eq!(err.field_path(), "tools[1].name_override");
            }
            other => panic!("expected InvalidToolConfig, got {other:?}"),
        }
    }
    #[test]
    fn tool_config_escape_hatch_invalid_name_override_fails_closed() {
        let v = serde_json::json!({
            "tool_config": {"tools": [
                {"id": "raw:ok", "name_override": "fine_name"},
                {"id": "raw:bad", "name_override": "not a tool id!"},
            ]},
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        match cfg.resolve(&all_known, true) {
            ResolvedToolset::InvalidToolConfig(err) => {
                assert_eq!(err.tool_id, "raw:bad");
                assert_eq!(err.field_path(), "tools[1].name_override");
            }
            other => panic!("expected InvalidToolConfig, got {other:?}"),
        }
        let v = serde_json::json!({
            "tool_config": {"tools": [{"id": "raw:ok", "name_override": "fine_name"}]},
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, true) else {
            panic!("valid escape-hatch config must resolve");
        };
        assert_eq!(resolved.toolset.tools.len(), 1);
    }
    #[test]
    fn invalid_entry_error_reports_wire_index_after_unknown_drop() {
        let v = serde_json::json!({
            "tools": [
                {"id": "wire:unknown"},
                {"id": "wire:bad", "params_json": "{not json"},
            ],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let known = |id: &str| id != "wire:unknown";
        match cfg.resolve(&known, false) {
            ResolvedToolset::InvalidToolConfig(err) => {
                assert_eq!(err.tool_id, "wire:bad");
                assert_eq!(
                    err.index, 1,
                    "index must be the wire position, not the known-subset position"
                );
            }
            other => panic!("expected InvalidToolConfig, got {other:?}"),
        }
    }
    #[test]
    fn valid_name_overrides_resolve_intact() {
        let v = serde_json::json!({
            "tools": [
                {"id": "wire:a", "name_override": "renamed_a"},
                {"id": "wire:b"},
            ],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, true) else {
            panic!("well-formed overrides must resolve to a toolset");
        };
        assert_eq!(resolved.toolset.tools.len(), 2);
        assert_eq!(
            resolved
                .toolset
                .tools
                .first()
                .and_then(|t| t.name_override.as_deref()),
            Some("renamed_a")
        );
        assert_eq!(
            resolved
                .toolset
                .tools
                .get(1)
                .and_then(|t| t.name_override.clone()),
            None
        );
    }
    #[test]
    fn pinned_tools_all_known_serves_full_expansion() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [{"id": "wire:tool"}],
            "manifest_version": "9.9.9-any",
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, false) else {
            panic!("known pinned tools must use the tools expansion");
        };
        assert!(resolved.unserved_tool_ids.is_empty());
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("wire:tool")
        );
    }
    /// Unknown ids must be partitioned and reported, never silently replaced by live preset resolution.
    #[test]
    fn pinned_tools_unknown_ids_are_partitioned_and_reported() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [
                {"id": "wire:known"},
                {"id": "wire:zz_unknown"},
                {"id": "wire:aa_unknown"},
            ],
            "manifest_version": "0.0.0-stale",
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let known = |id: &str| id == "wire:known";
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&known, false) else {
            panic!("partial coverage must still resolve to the known subset");
        };
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("wire:known")
        );
        assert_eq!(
            resolved.unserved_tool_ids,
            vec!["wire:aa_unknown".to_owned(), "wire:zz_unknown".to_owned()],
            "unserved ids are reported sorted"
        );
    }
    /// A fully-unknown expansion serves empty and reports every id; it never widens to preset/default.
    #[test]
    fn pinned_tools_all_unknown_serves_empty_and_reports_all() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [{"id": "wire:tool"}],
            "manifest_version": "0.0.0-stale",
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&none_known, false) else {
            panic!("all-unknown expansion must resolve (empty), not fall back");
        };
        assert!(resolved.toolset.tools.is_empty());
        assert_eq!(resolved.unserved_tool_ids, vec!["wire:tool".to_owned()]);
    }
    #[test]
    fn legacy_tools_without_manifest_version_are_not_gated() {
        let v = serde_json::json!({
            "preset": "explore",
            "tools": [{"id": "wire:tool"}],
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert!(cfg.manifest_version.is_none());
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&all_known, false) else {
            panic!("legacy unpinned tools must resolve without gating");
        };
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("wire:tool")
        );
    }
    #[test]
    fn tool_config_wins_regardless_of_stale_manifest_version() {
        let v = serde_json::json!({
            "tool_config": {"tools": [{"id": "raw:tool"}]},
            "tools": [{"id": "wire:tool"}],
            "manifest_version": "0.0.0-stale",
        });
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        let ResolvedToolset::Toolset(resolved) = cfg.resolve(&none_known, false) else {
            panic!("tool_config must always win");
        };
        assert!(resolved.unserved_tool_ids.is_empty());
        assert_eq!(resolved.toolset.tools.len(), 1);
        assert_eq!(
            resolved.toolset.tools.first().map(|t| t.id.as_str()),
            Some("raw:tool")
        );
    }
    #[test]
    fn malformed_tools_field_is_dropped_keeping_siblings() {
        let v = serde_json::json!({"preset": "explore", "tools": "not-a-list"});
        let cfg = WorkspaceBindConfig::from_metadata(&v);
        assert!(cfg.tools.is_none());
        assert!(matches!(
            cfg.resolve(&all_known, false),
            ResolvedToolset::UseDefault
        ));
        assert!(matches!(
            cfg.resolve(&all_known, true),
            ResolvedToolset::MissingToolConfig
        ));
    }
}
#[derive(Clone)]
pub struct BindMcpConfig {
    servers: Arc<[agent_client_protocol::McpServer]>,
    discovery_timeout: Duration,
    /// Server names designated first-party app endpoints — see
    /// [`Self::with_first_party_servers`].
    first_party: std::sync::Arc<std::collections::HashSet<String>>,
}
impl BindMcpConfig {
    pub const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
    /// Cap on how long a `session.bind` *waits* for the session's MCP convergence. The bind never blocks on MCP discovery — tools flow through dynamic registration and a slow convergence simply finishes after the bind — but waiting a short grace lets fast discovery land in the bind's own install, so the first turn usually sees the tools.
    /// The cap keeps the wait well under the hub's bind ack window ([`distill_tool_protocol::SESSION_BIND_ACK_TIMEOUT`], which the hub's ws router uses as its bind timeout; `bind_mcp_config_tests` pins the headroom), so one stalling MCP endpoint can never turn into a hub-visible bind failure.
    pub const MAX_BIND_CONVERGE_GRACE: Duration = Duration::from_secs(8);
    /// Headroom reserved out of the hub's bind-ack window for everything that is NOT the MCP converge wait: serializing the reply, the wire, and the hub's own handling.
    /// The converge grace is budgeted as `SESSION_BIND_ACK_TIMEOUT − elapsed setup − this margin`, so the TOTAL bind stays under the ack window no matter how slow session create / remount / the bind-mount hook were.
    pub const BIND_ACK_SAFETY_MARGIN: Duration = Duration::from_secs(1);
    /// Hard bound on configured servers. The config is externally influenced (a user-authored file on the desktop path), so its fan-out — child processes, connections, discovery work — carries an explicit cap; entries past it are dropped deterministically, in config order.
    pub const MAX_SERVERS: usize = 64;
    pub fn new(servers: impl IntoIterator<Item = agent_client_protocol::McpServer>) -> Self {
        let mut servers: Vec<_> = servers.into_iter().collect();
        crate::mcp::dedupe_servers_last_wins(&mut servers);
        crate::mcp::cap_servers(&mut servers);
        Self {
            servers: servers.into(),
            discovery_timeout: Self::DEFAULT_DISCOVERY_TIMEOUT,
            first_party: std::sync::Arc::new(std::collections::HashSet::new()),
        }
    }
    pub fn with_discovery_timeout(mut self, timeout: Duration) -> Self {
        self.discovery_timeout = timeout;
        self
    }
    /// Mark servers (by configured name) as FIRST-PARTY app endpoints: local desktop processes addressed by agent id.
    /// Only these receive the `X-Grok-Agent-ID` header (the bound session id) and the local-agent-endpoint transport posture (no OAuth probe, no proxy, no redirects).
    /// Defaults OFF for every server — a user-configured third-party MCP server must never receive the session id or lose its OAuth/proxy path.
    pub fn with_first_party_servers(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.first_party = std::sync::Arc::new(names.into_iter().collect());
        self
    }
    pub fn first_party_servers(&self) -> &std::collections::HashSet<String> {
        &self.first_party
    }
    pub fn servers(&self) -> &[agent_client_protocol::McpServer] {
        &self.servers
    }
    pub fn discovery_timeout(&self) -> Duration {
        self.discovery_timeout
    }
    /// How long a `session.bind` waits for the session's MCP convergence:
    /// [`Self::discovery_timeout`], capped by
    /// [`Self::MAX_BIND_CONVERGE_GRACE`].
    pub fn bind_converge_grace(&self) -> Duration {
        self.discovery_timeout.min(Self::MAX_BIND_CONVERGE_GRACE)
    }
    /// [`Self::bind_converge_grace`] as a DEADLINE from the start of bind handling rather than a duration started late: the grace only bounds the MCP wait, but the hub's ack window bounds the WHOLE bind — session create, path-virt remount, the bind-mount hook, and enrolment all spend from the same budget.
    /// Clamped at zero: a bind whose setup already consumed the window skips the wait entirely and lets discovery ride `tools_changed`.
    pub fn bind_converge_grace_within(&self, elapsed_since_bind_start: Duration) -> Duration {
        let remaining = distill_tool_protocol::SESSION_BIND_ACK_TIMEOUT
            .saturating_sub(Self::BIND_ACK_SAFETY_MARGIN)
            .saturating_sub(elapsed_since_bind_start);
        self.bind_converge_grace().min(remaining)
    }
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}
/// Top-level config required to construct a [`crate::handle::WorkspaceHandle`].
#[non_exhaustive]
pub struct WorkspaceConfig {
    /// Workspace root directory.
    pub root_cwd: PathBuf,
    /// Baseline tool config for the main session.
    pub default_tool_config: ToolServerConfig,
    /// Whether session-scoped fs operations should respect `.gitignore`.
    pub respect_gitignore: bool,
    /// Optional cross-session memory config.
    pub memory_config: Option<MemoryConfig>,
    /// Capacity of the workspace event broadcast channel.
    pub event_buffer_capacity: usize,
    /// Pluggable [`SessionContext`] / [`ToolRegistryBuilder`] producer.
    pub session_factory: Arc<dyn SessionContextFactory>,
    /// Global hook sources (e.g. `~/.claude/settings.json`, `~/.grok/hooks/`).
    pub hook_global_sources: Vec<HookSourceConfig>,
    /// Project-scoped hook sources (e.g. `<project>/.grok/hooks/`).
    pub hook_project_sources: Vec<HookSourceConfig>,
    /// Extra skill paths and a path-prefix ignore list. Stored on `WorkspaceShared` for `discover_skills` calls.
    pub skills_config: crate::discovery::SkillsConfig,
    /// CLI plugin dirs, config paths, and disabled/enabled lists. Stored on `WorkspaceShared` for `discover_plugins` calls.
    pub plugin_discovery_config: crate::discovery::PluginDiscoveryConfig,
    /// When `Some`, the workspace can connect to the server after construction via [`connect_hub`](crate::handle::WorkspaceHandle::connect_hub).
    pub hub_config: Option<HubConfig>,
    /// Auth provider for Grok service calls made from workspace-scoped code.
    /// `None` for workspaces that do not configure service auth.
    pub auth_provider: Option<distill_computer_hub_sdk::SharedAuthProvider>,
    /// Metadata attached to the tool server registration.
    /// Propagated through the server to `ServerInfo.metadata` in `servers.list` responses.
    /// Harness clients use it to identify the sandbox that started the tool server.
    pub server_metadata: Option<serde_json::Value>,
    /// Runtime-tunable timing/threshold config for the tool server.
    pub status_config: crate::status_config::StatusConfig,
    /// Folder-trust verdict for repo-local (project-scoped) LSP servers from `<cwd>/.grok/lsp.json`.
    /// `false` drops them at load, `true` keeps them.
    /// The shell caller resolves the verdict and threads it in; callers without a folder-trust decision pass `true`.
    pub project_lsp_trusted: bool,
    /// Fail `session.bind`s without an explicit toolset closed instead of widening to `default_tool_config`.
    /// Set by sandbox-launched standalone servers; local/CLI embedders keep the default-catalog fallback.
    pub require_explicit_toolset: bool,
    /// Confine `x.ai/fs/*` / `workspace.fs_*` resolution to the workspace root (reject `..`, absolute-outside-root, symlink escapes).
    /// Default `false` (unconfined); set to `true` only by the workspace server on a remote sandbox, where the root is a real tenant boundary.
    pub confine_fs_to_workspace_root: bool,
    /// MCP servers initialized for each admitted hub session bind.
    pub bind_mcp: Option<BindMcpConfig>,
    /// Whether hub tool calls wait for the session owner; resolved per host by
    /// [`approval_gate_for`](crate::permission::approval_gate_for).
    pub tool_approval: ToolApprovalGate,
    /// Which host runs this server; decides whether the root's `FsChanged` producer is lit.
    pub host_kind: crate::host_kind::WorkspaceHostKind,
}
/// Metadata a tool server announces so hub consumers can identify and route to it.
/// Re-export of the protocol crate's single catalog of well-known registration-metadata keys; every field is optional and independently sourced.
pub use distill_tool_protocol::ServerIdentityMetadata as WorkspaceServerMetadata;
/// Merge an env-sourced logical session id into caller-supplied tool-server metadata (`None` on the restore/local path).
/// Delegates to [`WorkspaceServerMetadata::merge_into`]: an explicit `session_id` already in `metadata` is never clobbered.
pub fn merge_session_metadata(
    metadata: Option<serde_json::Value>,
    env_session_id: Option<String>,
) -> Option<serde_json::Value> {
    WorkspaceServerMetadata {
        session_id: env_session_id.filter(|s| !s.is_empty()),
        ..Default::default()
    }
    .merge_into(metadata)
}
/// Merge the host-identity keys a standalone server can determine for itself (`host_kind`, `platform`) into caller-supplied metadata.
/// `host_kind` is one of the `distill_tool_protocol::HOST_KIND_*` values; the caller picks it because the same binary serves sandbox containers and headless user machines.
/// Delegates to [`WorkspaceServerMetadata::merge_into`]: explicit caller keys are never clobbered, and a non-object value passes through unchanged.
pub fn merge_host_identity_metadata(
    metadata: Option<serde_json::Value>,
    host_kind: &str,
) -> Option<serde_json::Value> {
    WorkspaceServerMetadata {
        host_kind: Some(host_kind.to_owned()),
        platform: Some(std::env::consts::OS.to_owned()),
        ..Default::default()
    }
    .merge_into(metadata)
}
impl WorkspaceConfig {
    /// Construct a minimal config suitable for proxy-mode workspaces where the workspace is used primarily as a ToolServer host.
    pub fn new_for_proxy(
        root_cwd: PathBuf,
        session_factory: Arc<dyn SessionContextFactory>,
        hub_config: HubConfig,
        auth_provider: distill_computer_hub_sdk::SharedAuthProvider,
        server_metadata: Option<serde_json::Value>,
        status_config: crate::status_config::StatusConfig,
        tool_config: ToolServerConfig,
    ) -> Self {
        Self {
            root_cwd,
            default_tool_config: tool_config,
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: crate::config::DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            auth_provider: Some(auth_provider),
            hub_config: Some(hub_config),
            server_metadata,
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
            bind_mcp: None,
            tool_approval: ToolApprovalGate::Off,
            status_config,
            host_kind: Default::default(),
        }
    }
}
/// Configuration for spawning a subagent session within a workspace.
#[derive(Clone)]
#[non_exhaustive]
pub struct AgentSessionConfig {
    /// Unique agent session id. Must be non-empty.
    pub agent_id: String,
    /// Filesystem isolation strategy.
    pub isolation: IsolationMode,
    /// Capability mode applied to this session's toolset.
    pub capability_mode: CapabilityMode,
    /// Per-fork tool override. `None` inherits from parent.
    pub tool_config: Option<ToolServerConfig>,
    /// Maximum recursion depth for subagent nesting.
    pub max_depth: u32,
    /// Working directory override. `None` inherits the parent's `cwd`.
    pub cwd_override: Option<PathBuf>,
    /// Extra env vars to layer on top of the parent's `session_env`.
    pub extra_env: HashMap<String, String>,
    /// Parent session to inherit from. Required.
    pub parent_session_id: Option<String>,
}
impl AgentSessionConfig {
    /// Construct a config with the supplied `agent_id` and otherwise minimal/permissive defaults.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            isolation: IsolationMode::None,
            capability_mode: CapabilityMode::ReadWrite,
            tool_config: None,
            max_depth: u32::MAX,
            cwd_override: None,
            extra_env: HashMap::new(),
            parent_session_id: None,
        }
    }
}
/// WARNING: `tool_config` is redacted from `Debug` output because `ToolServerConfig.tools[*].params` may contain credentials.
impl std::fmt::Debug for AgentSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSessionConfig")
            .field("agent_id", &self.agent_id)
            .field("isolation", &self.isolation)
            .field("capability_mode", &self.capability_mode)
            .field(
                "tool_config",
                if self.tool_config.is_some() {
                    &"Some(<redacted>)"
                } else {
                    &"None"
                },
            )
            .field("max_depth", &self.max_depth)
            .field("cwd_override", &self.cwd_override)
            .field("extra_env", &self.extra_env)
            .field("parent_session_id", &self.parent_session_id)
            .finish()
    }
}
/// A single hook source: either a JSON settings file or a directory of `*.json` hook files.
/// Maps 1:1 to [`distill_hooks::discovery::HookSource`] but uses owned `PathBuf` so the config struct is `'static`.
#[derive(Debug, Clone)]
pub enum HookSourceConfig {
    /// A single JSON settings file (e.g. `~/.claude/settings.json`).
    SettingsFile(PathBuf),
    /// A directory of `*.json` hook files (e.g. `~/.grok/hooks/`).
    Directory(PathBuf),
}
impl HookSourceConfig {
    pub fn as_hook_source(&self) -> HookSource<'_> {
        match self {
            Self::SettingsFile(path) => HookSource::SettingsFile(path),
            Self::Directory(path) => HookSource::Directory(path),
        }
    }
}
/// Filesystem isolation strategy for a forked session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationMode {
    /// No isolation: subagent shares the parent's working tree.
    #[default]
    None,
    /// Run the subagent in a copy-on-write git worktree.
    Worktree,
    /// Run the subagent inside a sandbox/container.
    Sandbox,
}
#[cfg(test)]
mod bind_mcp_config_tests {
    use super::BindMcpConfig;
    use std::time::Duration;
    /// The hub fails a `session.bind` whose ack takes longer than its window — the shared protocol constant, which the hub's ws router uses as its bind timeout.
    /// The grace a bind waits for the session's MCP convergence must stay under it with headroom, or one stalling MCP endpoint turns into a hub-visible bind failure.
    const HUB_BIND_ACK_TIMEOUT: Duration = distill_tool_protocol::SESSION_BIND_ACK_TIMEOUT;
    /// Duplicate names dedupe at the same chokepoint as the caps, LAST definition wins
    /// (JSON-object semantics): the session maps hold one slot per name, so without this,
    /// discovery would start one client per entry and drop all but one process without shutdown.
    #[test]
    fn duplicate_server_names_dedupe_last_wins() {
        let http = |name: &str, url: &str| {
            agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new(name, url).headers(vec![]),
            )
        };
        let config = BindMcpConfig::new([
            http("dup", "http://first.invalid/mcp"),
            http("other", "http://other.invalid/mcp"),
            http("dup", "http://last.invalid/mcp"),
        ]);
        let names: Vec<&str> = config
            .servers()
            .iter()
            .map(distill_mcp::servers::mcp_server_name)
            .collect();
        assert_eq!(
            names,
            ["other", "dup"],
            "one slot per name, positions of the kept (last) occurrences"
        );
        let Some(server) = config.servers().get(1) else {
            panic!("expected second server: {:?}", config.servers());
        };
        let agent_client_protocol::McpServer::Http(kept) = server else {
            panic!("expected http server");
        };
        assert_eq!(
            kept.url, "http://last.invalid/mcp",
            "the LAST definition wins"
        );
    }
    /// The config is externally influenced, so its server fan-out carries an
    /// explicit cap: entries past it are dropped deterministically, in
    /// config order.
    #[test]
    fn server_list_is_capped_deterministically() {
        let over = BindMcpConfig::MAX_SERVERS + 5;
        let config = BindMcpConfig::new((0..over).map(|index| {
            agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio::new(
                format!("server-{index:03}"),
                "/usr/bin/true",
            ))
        }));
        assert_eq!(config.servers().len(), BindMcpConfig::MAX_SERVERS);
        let Some(first_server) = config.servers().first() else {
            panic!("expected first server");
        };
        let Some(last_server) = config.servers().last() else {
            panic!("expected last server");
        };
        let first = distill_mcp::servers::mcp_server_name(first_server);
        let last = distill_mcp::servers::mcp_server_name(last_server);
        assert_eq!(first, "server-000");
        assert_eq!(
            last,
            format!("server-{:03}", BindMcpConfig::MAX_SERVERS - 1)
        );
    }
    #[test]
    fn bind_converge_grace_fits_under_the_hub_ack_window() {
        assert!(BindMcpConfig::MAX_BIND_CONVERGE_GRACE < HUB_BIND_ACK_TIMEOUT);
        let default_config = BindMcpConfig::new([]);
        assert!(default_config.bind_converge_grace() < HUB_BIND_ACK_TIMEOUT);
        let slow = BindMcpConfig::new([]).with_discovery_timeout(Duration::from_secs(300));
        assert!(slow.bind_converge_grace() < HUB_BIND_ACK_TIMEOUT);
        assert_eq!(slow.discovery_timeout(), Duration::from_secs(300));
        let fast = BindMcpConfig::new([]).with_discovery_timeout(Duration::from_secs(1));
        assert_eq!(fast.bind_converge_grace(), Duration::from_secs(1));
    }
    /// The grace is a deadline from the START of bind handling, not a duration started after setup: slow
    /// session create / remount / mount hook shrink it, and a bind whose setup already ate the ack window
    /// waits zero — so the TOTAL bind stays under the hub's ack no matter how slow the preceding steps were.
    #[test]
    fn bind_converge_grace_shrinks_when_setup_was_slow() {
        let config = BindMcpConfig::new([]);
        let full = config.bind_converge_grace();
        let budget = HUB_BIND_ACK_TIMEOUT - BindMcpConfig::BIND_ACK_SAFETY_MARGIN;
        assert_eq!(full, config.bind_converge_grace_within(Duration::ZERO));
        let slow_setup = Duration::from_secs(6);
        assert_eq!(
            (budget - slow_setup).min(full),
            config.bind_converge_grace_within(slow_setup)
        );
        assert!(config.bind_converge_grace_within(slow_setup) < full);
        assert_eq!(Duration::ZERO, config.bind_converge_grace_within(budget));
        assert_eq!(
            Duration::ZERO,
            config.bind_converge_grace_within(HUB_BIND_ACK_TIMEOUT + Duration::from_secs(5))
        );
        for secs in 0..=12 {
            let elapsed = Duration::from_secs(secs);
            assert!(
                config.bind_converge_grace_within(elapsed) <= budget.saturating_sub(elapsed),
                "the grace at elapsed {elapsed:?} must fit the remaining ack budget"
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{WorkspaceServerMetadata, merge_host_identity_metadata, merge_session_metadata};
    #[test]
    fn merge_host_identity_metadata_announces_kind_and_platform_without_clobbering() {
        let merged = merge_host_identity_metadata(
            Some(serde_json::json!({ "session_id": "sess-1", "host_kind": "spaceship" })),
            distill_tool_protocol::HOST_KIND_SANDBOX,
        )
        .unwrap();
        assert_eq!(
            merged,
            serde_json::json!({
                "session_id": "sess-1",
                "host_kind": "spaceship",
                "platform": std::env::consts::OS,
            })
        );
        let fresh =
            merge_host_identity_metadata(None, distill_tool_protocol::HOST_KIND_DAEMON).unwrap();
        assert_eq!(
            fresh,
            serde_json::json!({ "host_kind": "daemon", "platform": std::env::consts::OS })
        );
        let scalar = serde_json::json!("opaque");
        assert_eq!(
            merge_host_identity_metadata(Some(scalar.clone()), distill_tool_protocol::HOST_KIND_DAEMON)
                .unwrap(),
            scalar
        );
    }
    #[test]
    fn workspace_server_metadata_serializes_all_present_fields() {
        let meta = WorkspaceServerMetadata {
            sandbox_id: Some("sb-123".to_owned()),
            session_id: Some("11111111-1111-1111-1111-111111111111".to_owned()),
            provider_id: Some("test-provider".to_owned()),
            launch_id: Some("33333333-3333-3333-3333-333333333333".to_owned()),
            ..Default::default()
        };
        let value = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "sandbox_id": "sb-123",
                "session_id": "11111111-1111-1111-1111-111111111111",
                "provider_id": "test-provider",
                "launch_id": "33333333-3333-3333-3333-333333333333",
            })
        );
    }
    #[test]
    fn workspace_server_metadata_omits_none_fields() {
        let meta = WorkspaceServerMetadata {
            sandbox_id: Some("sb-123".to_owned()),
            ..Default::default()
        };
        let value = serde_json::to_value(&meta).unwrap();
        assert_eq!(value, serde_json::json!({ "sandbox_id": "sb-123" }));
        let empty = serde_json::to_value(WorkspaceServerMetadata::default()).unwrap();
        assert_eq!(empty, serde_json::json!({}));
    }
    #[test]
    fn workspace_server_metadata_deserializes_legacy_payload_without_new_fields() {
        let legacy = serde_json::json!({
            "sandbox_id": "sb-legacy",
            "cwd": "/workspace",
            "mode": "remote",
        });
        let meta: WorkspaceServerMetadata = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.sandbox_id.as_deref(), Some("sb-legacy"));
        assert_eq!(meta.cwd.as_deref(), Some("/workspace"));
        assert_eq!(meta.session_id, None);
        assert_eq!(meta.provider_id, None);
    }
    #[test]
    fn workspace_server_metadata_round_trips_with_new_fields() {
        let meta = WorkspaceServerMetadata {
            sandbox_id: Some("sb-123".to_owned()),
            session_id: Some("22222222-2222-2222-2222-222222222222".to_owned()),
            provider_id: Some("test-provider".to_owned()),
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: WorkspaceServerMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sandbox_id, meta.sandbox_id);
        assert_eq!(back.session_id, meta.session_id);
        assert_eq!(back.provider_id, meta.provider_id);
    }
    #[test]
    fn workspace_server_metadata_deserializes_partial_new_fields() {
        let only_session = serde_json::json!({
            "sandbox_id": "sb-1",
            "session_id": "33333333-3333-3333-3333-333333333333",
        });
        let meta: WorkspaceServerMetadata = serde_json::from_value(only_session).unwrap();
        assert_eq!(
            meta.session_id.as_deref(),
            Some("33333333-3333-3333-3333-333333333333")
        );
        assert_eq!(meta.provider_id, None);
        let only_provider = serde_json::json!({
            "sandbox_id": "sb-1",
            "provider_id": "test-provider",
        });
        let meta: WorkspaceServerMetadata = serde_json::from_value(only_provider).unwrap();
        assert_eq!(meta.provider_id.as_deref(), Some("test-provider"));
        assert_eq!(meta.session_id, None);
    }
    #[test]
    fn workspace_server_metadata_reads_start_path_shaped_payload() {
        let start_path = serde_json::json!({
            "cwd": "/workspace",
            "mode": "remote",
            "sandbox_id": "sb-start",
            "session_id": "44444444-4444-4444-4444-444444444444",
            "provider_id": "test-provider",
        });
        let meta: WorkspaceServerMetadata = serde_json::from_value(start_path).unwrap();
        assert_eq!(meta.sandbox_id.as_deref(), Some("sb-start"));
        assert_eq!(
            meta.session_id.as_deref(),
            Some("44444444-4444-4444-4444-444444444444")
        );
        assert_eq!(meta.provider_id.as_deref(), Some("test-provider"));
    }
    #[test]
    fn merge_session_metadata_builds_struct_from_env_on_none_branch() {
        let merged = merge_session_metadata(None, Some("sess-1".to_owned())).unwrap();
        assert_eq!(merged, serde_json::json!({ "session_id": "sess-1" }));
        let empty = merge_session_metadata(None, None).unwrap();
        assert_eq!(empty, serde_json::json!({}));
    }
    #[test]
    fn merge_session_metadata_overlays_into_object_without_clobbering() {
        let base = serde_json::json!({ "sandbox_id": "sb-9", "mode": "remote" });
        let merged = merge_session_metadata(Some(base), Some("env-id".to_owned())).unwrap();
        assert_eq!(
            merged,
            serde_json::json!({
                "sandbox_id": "sb-9",
                "mode": "remote",
                "session_id": "env-id",
            })
        );
        let explicit = serde_json::json!({ "session_id": "explicit" });
        let merged = merge_session_metadata(Some(explicit), Some("env-id".to_owned())).unwrap();
        assert_eq!(merged, serde_json::json!({ "session_id": "explicit" }));
    }
    #[test]
    fn merge_session_metadata_leaves_object_untouched_when_no_env_id() {
        let base = serde_json::json!({ "sandbox_id": "sb-9" });
        let merged = merge_session_metadata(Some(base.clone()), None).unwrap();
        assert_eq!(merged, base);
    }
    #[test]
    fn merge_session_metadata_non_object_is_returned_unchanged() {
        let scalar = serde_json::json!("just-a-string");
        let merged =
            merge_session_metadata(Some(scalar.clone()), Some("env-id".to_owned())).unwrap();
        assert_eq!(merged, scalar);
    }
    #[test]
    fn merge_session_metadata_treats_empty_env_id_as_absent() {
        let none_branch = merge_session_metadata(None, Some(String::new())).unwrap();
        assert_eq!(none_branch, serde_json::json!({}));
        let base = serde_json::json!({ "sandbox_id": "sb-9" });
        let overlay = merge_session_metadata(Some(base.clone()), Some(String::new())).unwrap();
        assert_eq!(overlay, base);
    }
    #[test]
    fn workspace_server_metadata_rejects_wrong_typed_field() {
        let bad = serde_json::json!({ "sandbox_id": "sb-1", "session_id": 42 });
        let result: Result<WorkspaceServerMetadata, _> = serde_json::from_value(bad);
        assert!(result.is_err());
    }
    /// The lenient reader (`server_metadata_typed`'s path) salvages per field where the strict deserialize above fails wholesale.
    #[test]
    fn workspace_server_metadata_from_metadata_salvages_wrong_typed_sibling() {
        let bad = serde_json::json!({ "sandbox_id": "sb-1", "session_id": 42 });
        let meta = WorkspaceServerMetadata::from_metadata(&bad);
        assert_eq!(meta.sandbox_id.as_deref(), Some("sb-1"));
        assert_eq!(meta.session_id, None);
        assert_eq!(
            WorkspaceServerMetadata::from_metadata(&serde_json::json!("opaque")),
            WorkspaceServerMetadata::default()
        );
    }
}
