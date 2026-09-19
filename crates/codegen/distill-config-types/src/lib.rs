// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#![deny(clippy::indexing_slicing)]
mod flags;
pub use flags::*;
mod registry;
pub use registry::*;
mod memory;
pub use memory::*;
mod mcp;
pub use mcp::*;
mod permission;
pub use permission::*;
mod auth_provider;
pub use auth_provider::*;
use serde::{Deserialize, Serialize};
use distill_announcements::RemoteAnnouncement;
pub use distill_config::DisplayRefreshSettings;
use distill_config::deserialize::optional_bool as de_opt_bool_tolerant;
/// A remote `campaigns[]` entry: an `id` gate plus a flattened patch that can set any config key.
/// It is the JSON sibling of a `[[campaigns]]` TOML override.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CampaignOverride {
    #[serde(default, alias = "campaign_id")]
    pub id: Option<String>,
    #[serde(flatten, default)]
    pub patch: serde_json::Map<String, serde_json::Value>,
}
/// Doom-loop recovery settings: one struct serves both the local `[doom_loop_recovery]` TOML table and the remote `doom_loop_recovery` JSON object.
/// Every field is `Option` with a per-field default, so a partial object parses and unknown future keys are ignored.
/// Unset fields fall through per-field in `resolve_doom_loop_recovery`: env, then TOML, then remote, then default.
/// The namespace is distinct from the removed legacy `doom_loop_*` keys.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct DoomLoopRecoverySettings {
    /// Send the `x-grok-doom-loop-check` header, parse the reported triggers, and resample confident loops.
    /// `Some(false)` is a kill-switch; absent uses the client default (on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Highest `tail_repetition` threshold considered confident (clamped to 2..=64); absent uses the client default (64).
    /// The server emits every fired threshold, and the client filters the returned trigger labels with this.
    /// It is never sent as a request parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_threshold: Option<u32>,
    /// Resample budget per turn (clamped to 0..=5); absent uses the client default (2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Detector window sent as the value of `x-grok-doom-loop-check` (honored in 512..=4096, otherwise 4096; absent uses the client default, 1024).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_tokens: Option<u32>,
}
/// Per-kind age policy for auto-GC: seconds or never.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeKindMaxAge {
    Secs(u64),
    Never,
}
impl Serialize for WorktreeKindMaxAge {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Secs(n) => serializer.serialize_u64(*n),
            Self::Never => serializer.serialize_str("never"),
        }
    }
}
impl<'de> Deserialize<'de> for WorktreeKindMaxAge {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = WorktreeKindMaxAge;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("u64 seconds or \"never\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(WorktreeKindMaxAge::Secs(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(WorktreeKindMaxAge::Secs)
                    .map_err(E::custom)
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.eq_ignore_ascii_case("never") {
                    Ok(WorktreeKindMaxAge::Never)
                } else if let Ok(n) = v.parse::<u64>() {
                    Ok(WorktreeKindMaxAge::Secs(n))
                } else {
                    Err(E::custom("expected \"never\" or integer seconds"))
                }
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(WorktreeKindMaxAge::Never)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(WorktreeKindMaxAge::Never)
            }
        }
        deserializer.deserialize_any(V)
    }
}
/// Policy from the local `[worktree.auto_gc]` table or the remote `worktree_auto_gc` object.
/// Each field deserializes tolerantly, so one bad key cannot drop a sibling kill-switch.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorktreeAutoGcSettings {
    /// `Some(false)` is a kill-switch; absent defaults to on (an env kill still applies).
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    /// Age cutoff in seconds, on platforms where expiring by age is allowed; the resolver clamps it.
    #[serde(
        default,
        deserialize_with = "de_opt_u64_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_age_secs: Option<u64>,
    /// Minimum seconds between successful auto-GC runs; the resolver clamps it.
    #[serde(
        default,
        deserialize_with = "de_opt_u64_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_interval_secs: Option<u64>,
    /// When set, counts worktrees old enough to delete without deleting them.
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub dry_run: Option<bool>,
    /// Only has an effect on Linux.
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_orphan_snapshots: Option<bool>,
    /// Per-kind max ages (`session`, `ab`, `pool`, `fork`, `manual`, `subagent`), each seconds or `"never"`.
    /// Absent keys use the defaults; the client default keeps `manual` at never.
    /// Remote may set `manual` to a finite TTL, and local TOML can restore `"never"`.
    /// Unknown kind keys are ignored at resolve time.
    #[serde(
        default,
        deserialize_with = "de_opt_max_age_by_kind_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_age_by_kind: Option<std::collections::BTreeMap<String, WorktreeKindMaxAge>>,
    /// Also rebuild discovery and scrub grok's stale `.git/worktrees/` entries (default off).
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_rebuild: Option<bool>,
    /// Seconds between rebuilds, throttled independently of GC; absent means 24h. It is clamped like `min_interval_secs`.
    #[serde(
        default,
        deserialize_with = "de_opt_u64_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub rebuild_min_interval_secs: Option<u64>,
}
fn de_opt_u64_tolerant<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("u64 (wrong types ignored)")
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v))
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(u64::try_from(v).ok())
        }
        fn visit_u32<E: serde::de::Error>(self, v: u32) -> Result<Self::Value, E> {
            Ok(Some(u64::from(v)))
        }
        fn visit_i32<E: serde::de::Error>(self, v: i32) -> Result<Self::Value, E> {
            Ok(u64::try_from(v).ok())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<A: serde::de::Deserializer<'de>>(
            self,
            d: A,
        ) -> Result<Self::Value, A::Error> {
            d.deserialize_any(V)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
    }
    deserializer.deserialize_any(V)
}
/// A bad whole value becomes `None`; a bad per-entry value is skipped and its siblings kept.
fn de_opt_max_age_by_kind_tolerant<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<std::collections::BTreeMap<String, WorktreeKindMaxAge>>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in map {
                if let Ok(age) = serde_json::from_value::<WorktreeKindMaxAge>(v) {
                    out.insert(k, age);
                }
            }
            Ok(Some(out))
        }
        _ => Ok(None),
    }
}
/// A nested value that is present but malformed becomes `None` with a warning, so one bad setting cannot fail the whole [`RemoteSettings`] parse.
pub fn deserialize_tolerant<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let Some(value) =
        Option::<serde_json::Value>::deserialize(deserializer)?.filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                setting = std::any::type_name::<T>(),
                "ignoring malformed remote setting; falling through to local defaults"
            );
            Ok(None)
        }
    }
}
/// Consent notice from `distill_settings.consent_gate`.
/// The server decides which accounts see it; the payload carries no targeting.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConsentGate {
    /// The id keys the stored answer and the upstream record. A payload without one is dropped whole.
    pub id: String,
    /// Acceptance version for this notice. It only compares against an answer to the same `id`.
    #[serde(default)]
    pub version: Option<i32>,
    #[serde(default)]
    pub title: Option<String>,
    /// `[label](https://…)` links are painted inline; there is no scrolling, so keep it short.
    #[serde(default)]
    pub body: Option<String>,
    /// This button is the only way past the notice.
    #[serde(default)]
    pub accept_label: Option<String>,
}
/// Remote settings fetched from cli-chat-proxy `GET /v1/settings`.
///
/// All fields are `Option` with `#[serde(default)]` so that:
/// - Missing fields from old servers are ignored
/// - New fields added in the future don't break existing clients
/// - Callers can distinguish "server said false" from "server didn't say"
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteSettings {
    /// When `Some(true)`, the server recommends enabling leader mode.
    /// It is the fallback when the user hasn't set `[cli] use_leader` locally.
    #[serde(default)]
    pub leader_mode: Option<bool>,
    #[serde(default)]
    pub max_upload_file_bytes: Option<u64>,
    #[serde(default)]
    pub max_upload_untracked_bytes: Option<u64>,
    /// When `Some(true)`, capture workspace files for non-git project dirs (client default: off).
    #[serde(default)]
    pub non_git_workspace_capture: Option<bool>,
    #[serde(default)]
    pub login_shell_capture: Option<bool>,
    /// Fleet-wide kill switch for turn-level transient retries; it applies at the next spawn, and local config or env wins.
    /// Malformed values must not fail the parse.
    #[serde(default, deserialize_with = "de_opt_bool_tolerant")]
    pub turn_transient_retry: Option<bool>,
    /// Kill switch for the credential-less-401 park: `Some(false)` restores terminal behavior;
    /// absent = enabled. Resolved at spawn; running turns finish on their spawn-time value.
    #[serde(default, deserialize_with = "de_opt_bool_tolerant")]
    pub uncharged_401_park: Option<bool>,
    /// Release channel: `"stable"` or `"alpha"`.
    /// It is the fallback when no local `[cli] channel` or `--alpha`/`--stable` flag is set.
    #[serde(default)]
    pub release_channel: Option<String>,
    /// When `Some(true)`, enable LOC attribution tracking for this session.
    #[serde(default)]
    pub loc_tracking: Option<bool>,
    /// Legacy remote memory toggle, used only when memory v2 is not enabled.
    #[serde(default)]
    pub memory_enabled: Option<bool>,
    /// Dedicated memory-v2 settings object. This is isolated from the legacy
    /// memory fields and populated from `distill_memory_v2_*` features.
    #[serde(default)]
    pub memory_v2: Option<memory::MemoryV2Settings>,
    #[serde(default)]
    pub memory_search_max_results: Option<u32>,
    #[serde(default)]
    pub memory_search_min_score: Option<f32>,
    #[serde(default)]
    pub memory_initial_injection_enabled: Option<bool>,
    #[serde(default)]
    pub memory_initial_injection_min_score: Option<f32>,
    #[serde(default)]
    pub memory_embedding_model: Option<String>,
    #[serde(default)]
    pub memory_embedding_dimensions: Option<u32>,
    #[serde(default)]
    pub pruning_enabled: Option<bool>,
    #[serde(default)]
    pub pruning_keep_last_n_turns: Option<u32>,
    #[serde(default)]
    pub pruning_soft_trim_threshold: Option<u32>,
    #[serde(default)]
    pub flush_enabled: Option<bool>,
    #[serde(default)]
    pub flush_soft_threshold_tokens: Option<u64>,
    #[serde(default)]
    pub flush_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub flush_semantic_dedup_threshold: Option<f64>,
    #[serde(default)]
    pub memory_temporal_decay_enabled: Option<bool>,
    #[serde(default)]
    pub memory_temporal_decay_half_life_days: Option<f64>,
    #[serde(default)]
    pub memory_mmr_enabled: Option<bool>,
    #[serde(default)]
    pub memory_mmr_lambda: Option<f64>,
    #[serde(default)]
    pub memory_watcher_enabled: Option<bool>,
    #[serde(default)]
    pub dream_enabled: Option<bool>,
    #[serde(default)]
    pub dream_min_hours: Option<u64>,
    #[serde(default)]
    pub dream_min_sessions: Option<u64>,
    #[serde(default)]
    pub dream_check_interval_secs: Option<u64>,
    /// Cadence (seconds) of the pager's watch for a free account becoming paid.
    /// `0` disables it; the pager clamps and defaults (see its `app::subscription` module).
    /// It arrives from the `distill_settings` remote settings flag via the cli-chat-proxy `/settings` flatten catch-all.
    #[serde(default)]
    pub subscription_watch_interval_secs: Option<u64>,
    #[serde(default)]
    pub writeback_enabled: Option<bool>,
    /// OAuth2 provider issuer URL (e.g., "https://auth.x.ai").
    /// When present together with `oauth2_client_id`, the client uses the OAuth2 authorization code flow.
    /// Remote settings control it so the rollout can be gradual.
    #[serde(default)]
    pub oauth2_issuer: Option<String>,
    /// OAuth2 client_id for the CLI. It pairs with `oauth2_issuer`.
    #[serde(default)]
    pub oauth2_client_id: Option<String>,
    /// When `Some(true)`, enables grok's default OAuth2 (Grok auth.x.ai).
    /// Enterprise OIDC (user's own IdP via `oidc` config) always wins.
    /// The `--oauth` CLI flag overrides it.
    #[serde(default)]
    pub grok_oauth_enabled: Option<bool>,
    #[serde(default)]
    pub lsp_tools_enabled: Option<bool>,
    /// Remote kill-switch and default for the folder-trust gate.
    /// The gate decides whether repo-local MCP/LSP servers (commands from working-tree config files) need a per-folder trust decision before they spawn.
    /// `Some(true)` enables, `Some(false)` is a kill-switch, `None` falls back to the client default (on).
    /// It sits below env `GROK_FOLDER_TRUST`, user `[folder_trust] enabled`, and managed config in the resolver chain.
    /// See `agent::folder_trust::feature_enabled`.
    #[serde(default)]
    pub folder_trust_enabled: Option<bool>,
    #[serde(default)]
    pub write_file_enabled: Option<bool>,
    #[serde(default)]
    pub active_agent_messages_enabled: Option<bool>,
    /// File toolset: `"standard"` or `"hashline"`.
    /// This is the server-side default; local `[toolset] file_toolset` in config.toml takes precedence when set.
    #[serde(default)]
    pub file_toolset: Option<String>,
    /// Per-chunk idle timeout in seconds for inference streaming.
    /// It is the fallback when no per-model `inference_idle_timeout_secs` is set in config.toml.
    #[serde(default)]
    pub inference_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub subagent_rate_limit_max_attempts: Option<u32>,
    #[serde(default)]
    pub repo_status_in_system_prompt: Option<bool>,
    /// Global default MCP startup-handshake timeout (seconds).
    /// It is the lowest-precedence fallback; per-server config, env, and requirements/managed override it.
    #[serde(default)]
    pub mcp_startup_timeout_secs: Option<u64>,
    /// Global default MCP tool-result inline cap (bytes), from remote settings `distill_settings.max_mcp_output_bytes`.
    /// Requirements, env, and `config.toml [mcp] max_output_bytes` override it. The built-in default is 20_000.
    #[serde(default)]
    pub max_mcp_output_bytes: Option<u64>,
    /// When `Some(true)`, enable session registry hooks (register, update, finalize, memory upload).
    /// When absent or `Some(false)`, all hooks are disabled.
    #[serde(default)]
    pub session_registry_enabled: Option<bool>,
    /// The remote settings `doom_loop_recovery` JSON object; see [`DoomLoopRecoverySettings`].
    /// Absent means every knob falls through to TOML/defaults; a partial object falls through per-field.
    #[serde(default)]
    pub doom_loop_recovery: Option<DoomLoopRecoverySettings>,
    /// Automatic worktree GC policy; see [`WorktreeAutoGcSettings`].
    /// Absent means every knob falls through to TOML/defaults; a partial object falls through per-field.
    /// A value that is present but malformed drops to `None` instead of failing the whole `RemoteSettings` parse.
    /// Which platforms may expire worktrees by age is hardcoded in the client; remote cannot override it.
    #[serde(default, deserialize_with = "deserialize_tolerant")]
    pub worktree_auto_gc: Option<WorktreeAutoGcSettings>,
    /// Enable/disable the runtime turn-end TodoGate remotely.
    /// Precedence: CLI `--todo-gate`, then this field, then the built-in default (`false`).
    /// The gate ships disabled; set this to `Some(true)` (via the `distill_settings` remote settings key) to enable it.
    /// See `session::acp_session::resolve_reminder_policy`.
    #[serde(default)]
    pub todo_gate_enabled: Option<bool>,
    /// Hard cap on TodoGate fires per user prompt.
    /// Precedence: this field, then the built-in default (`DEFAULT_TODO_GATE_MAX_FIRES`).
    /// There is no CLI override. See `session::acp_session::resolve_reminder_policy`.
    #[serde(default)]
    pub todo_gate_max_fires_per_prompt: Option<u32>,
    /// Length-salvage continue budget for `max_tokens`-truncated turns.
    /// `Some(0)` is explicit off and kills every tier, including the
    /// always-on cursor one and the `GROK_LENGTH_SALVAGE` env opt-in.
    /// Otherwise: cursor tier > env opt-in > this field > off. See
    /// `session::acp_session_impl::length_salvage`.
    #[serde(default)]
    pub length_salvage_budget: Option<u32>,
    #[serde(default)]
    pub auto_wake_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_skills_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_rules_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_agents_enabled: Option<bool>,
    #[serde(default)]
    pub claude_skills_enabled: Option<bool>,
    #[serde(default)]
    pub claude_rules_enabled: Option<bool>,
    #[serde(default)]
    pub claude_agents_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_mcps_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_hooks_enabled: Option<bool>,
    #[serde(default)]
    pub claude_mcps_enabled: Option<bool>,
    #[serde(default)]
    pub claude_hooks_enabled: Option<bool>,
    #[serde(default)]
    pub cursor_sessions_enabled: Option<bool>,
    #[serde(default)]
    pub claude_sessions_enabled: Option<bool>,
    #[serde(default)]
    pub codex_sessions_enabled: Option<bool>,
    /// When `Some(true)`, enable goal mode remotely.
    /// When `Some(false)`, force-disable it (kill-switch).
    /// Absent uses the client default (enabled).
    #[serde(default)]
    pub goal_enabled: Option<bool>,
    /// When `Some(true)`, enable the goal-completion classifier remotely.
    /// When `Some(false)`, force-disable it.
    /// Absent tracks goal mode: enabled exactly when goal mode is on.
    #[serde(default)]
    pub goal_classifier_enabled: Option<bool>,
    /// When `Some(true)`, enable the goal planner remotely.
    /// When `Some(false)`, force-disable it.
    /// Absent tracks goal mode: enabled exactly when goal mode is on.
    #[serde(default)]
    pub goal_planner_enabled: Option<bool>,
    /// When `Some(true)`, enable the goal summarizer remotely (the one-shot closing "what was accomplished" summary on a verified achievement).
    /// When `Some(false)`, force-disable it (kill-switch).
    /// Absent tracks goal mode: enabled exactly when goal mode is on.
    #[serde(default)]
    pub goal_summary_enabled: Option<bool>,
    /// Number of adversarial skeptics spawned per goal-verification attempt (step 2 of the staged gate); clamped to `1..=5` at the resolver.
    /// Absent uses the harness default, `goal_classifier::GOAL_VERIFIER_SKEPTIC_COUNT` (3 today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_verifier_count: Option<u32>,
    /// Maximum per-goal classifier runs before the goal auto-pauses (BackOff); clamped to `1..=10` at the resolver.
    /// Absent uses the harness default, `goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT` (3 today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_classifier_max_runs: Option<u32>,
    /// Fire the stall-triggered strategist every N consecutive `NotAchieved` verifications; clamped to `>= 1` at the resolver.
    /// Absent defaults to `max(1, goal_classifier_max_runs / 2)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_strategist_every: Option<u32>,
    /// Planner role model and toolset. Absent inherits the current model.
    /// A malformed value drops to `None` rather than failing the whole `RemoteSettings` payload (see [`deserialize_tolerant_goal_role_model`]).
    #[serde(
        default,
        deserialize_with = "deserialize_tolerant_goal_role_model",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal_planner_model: Option<GoalRoleModel>,
    /// Strategist role model and toolset. Absent inherits the current model.
    /// A malformed value drops to `None` (see [`deserialize_tolerant_goal_role_model`]).
    #[serde(
        default,
        deserialize_with = "deserialize_tolerant_goal_role_model",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal_strategist_model: Option<GoalRoleModel>,
    /// Ordered skeptic pool: `pool[0]` is skeptic-0's model, and skeptics `1..N` are assigned round-robin over the pool.
    /// Empty or absent inherits the current model.
    /// A single malformed pool entry is dropped rather than discarding the whole pool (see [`deserialize_tolerant_goal_skeptic_models`]).
    #[serde(
        default,
        deserialize_with = "deserialize_tolerant_goal_skeptic_models",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub goal_skeptic_models: Vec<GoalRoleModel>,
    #[serde(default)]
    pub workflows_enabled: Option<bool>,
    /// Remote fallback for managed MCP connector fetching.
    #[serde(default)]
    pub managed_mcps_enabled: Option<bool>,
    #[serde(default)]
    pub managed_mcp_gateway_tools_enabled: Option<bool>,
    /// Remote-policy disable lever for the external OTEL stream (customer collectors); feeds `ExternalOtelRemotePolicy.force_disable`.
    /// Tighten-only: there is no `external_otel_enabled` remote field and `apply_remote_policy` never enables, so even a stale disk cache can only restrict.
    /// Org-wide enable ships via managed config instead.
    #[serde(default)]
    pub external_otel_disabled: Option<bool>,
    /// Force the external stream's content gates (`OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_DETAILS`) off regardless of local env/config.
    /// It is tighten-only, like `external_otel_disabled`.
    #[serde(default)]
    pub external_otel_content_gates_locked: Option<bool>,
    /// `Some(false)` disables managed-config signature verification (remote kill-switch).
    #[serde(default)]
    pub managed_config_signature_verification: Option<bool>,
    #[serde(default)]
    pub telemetry_enabled: Option<bool>,
    /// Telemetry mode override (string): `"session-metrics"`, `"full"`, `"off"`.
    /// It takes precedence over `telemetry_enabled` (bool) when present.
    #[serde(default)]
    pub telemetry_mode: Option<String>,
    #[serde(default)]
    pub trace_upload_enabled: Option<bool>,
    /// Request body encodings the server accepts on the chat routes. Empty or absent means plain JSON only.
    #[serde(default)]
    pub accept_request_encodings: Vec<RemoteRequestEncoding>,
    /// Enable user-facing feedback (heuristic popups, `/feedback` command).
    /// Session analytics (signal sync, turn deltas) are gated separately by `telemetry_enabled`.
    #[serde(default)]
    pub feedback_enabled: Option<bool>,
    /// Gradual rollout of the `/feedback` trace-consent card.
    #[serde(default)]
    pub feedback_trace_card_enabled: Option<bool>,
    /// Two-pass (prefire) compaction.
    /// Near the auto-compact threshold the shell speculatively summarizes the history prefix in the background (pass 1 produces an interim note).
    /// At compaction it summarizes that note plus the recent tail (pass 2), keeping summarizer latency off the critical path.
    /// `Some(false)` forces off; `None` falls through env, `[features]`, then the default (on).
    #[serde(default)]
    pub two_pass_compaction_enabled: Option<bool>,
    /// Dynamic tip list from remote settings. When present with non-empty entries, one tip is shown at startup (rotated daily by UTC day).
    /// `None` or `[]` shows no tips.
    #[serde(default)]
    pub tips: Option<Vec<String>>,
    /// Free-form per-command tags (e.g. `new`, `beta`) rendered as a bracketed label in the slash dropdown, keyed by canonical command name.
    /// A malformed value drops to `None` without failing the whole parse; local `[slash_command_tags]` overrides per key.
    /// See `resolve_slash_command_tags`.
    #[serde(default, deserialize_with = "deserialize_tolerant")]
    pub slash_command_tags: Option<std::collections::BTreeMap<String, String>>,
    /// When present, controls the non-Git-repo warning at session start.
    /// It arrives as `non_git_warning` in `distill_settings`.
    /// It takes precedence over `[features] non_git_warning` in config.toml.
    /// `Some(true)` enables, `Some(false)` acts as a kill-switch, `None` falls back to local config.
    #[serde(default)]
    pub non_git_warning: Option<bool>,
    /// Remote settings gate for first-run auto-registration of the official Grok marketplace source.
    /// `Some(true)` enables, `Some(false)` is a kill-switch, `None` falls back to env/default (off).
    #[serde(default)]
    pub official_marketplace_auto_register: Option<bool>,
    /// Remote settings gate for the inline plugin-install CTA (the keyword-matched marketplace upsell above the prompt).
    /// `Some(true)` enables, `Some(false)` is a kill-switch, `None` falls back to env/default (off).
    #[serde(default)]
    pub plugin_cta: Option<bool>,
    /// Remote announcements list from proxy. Malformed items are skipped entirely.
    /// `None` or `[]` displays no announcements.
    #[serde(default, deserialize_with = "deserialize_tolerant_announcements")]
    pub announcements: Option<Vec<RemoteAnnouncement>>,
    #[serde(default)]
    pub web_search_model: Option<String>,
    #[serde(default)]
    pub session_summary_model: Option<String>,
    #[serde(default)]
    pub image_description_model: Option<String>,
    /// Server-side pin for the next-prompt suggestion model (tab-autocomplete ghost text), from the `distill_settings` remote settings flag.
    /// It sits below env (`GROK_PROMPT_SUGGESTIONS_MODEL`) and `[models] prompt_suggestion` in config.toml.
    /// It sits above the client hint and the built-in `grok-4.6` default.
    /// When the effective model is not in the shell's model catalog the suggestion request is skipped entirely; the session model is never used instead.
    /// See `ModelOverrideConfig::resolve` and `handle_suggest_prompt`.
    #[serde(default)]
    pub prompt_suggestion_model: Option<String>,
    /// Server-recommended default model ID for new sessions.
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub campaigns: Vec<CampaignOverride>,
    /// When `Some(true)`, foreground commands that hit the default timeout are auto-backgrounded instead of killed.
    /// It is the fallback when no local `[toolset.bash] auto_background_on_timeout` is set in config.toml.
    #[serde(default)]
    pub auto_background_on_timeout: Option<bool>,
    /// When `Some(false)`, foreground commands containing a background `&` operator are rejected.
    /// It is the fallback when no local `[toolset.bash] allow_background_operator` is set; absent uses the client default (allow).
    #[serde(default)]
    pub allow_background_operator: Option<bool>,
    /// Remote settings fallback for `[toolset.ask_user_question] timeout_enabled`.
    /// When `Some(false)`, questionnaires wait forever unless a higher tier (requirements, env, user, or managed config) sets otherwise.
    #[serde(default)]
    pub ask_user_question_timeout_enabled: Option<bool>,
    /// Remote settings fallback for `[toolset.ask_user_question] timeout_secs` (positive seconds).
    /// Absent uses the client default (1800 seconds, 30 minutes).
    #[serde(default)]
    pub ask_user_question_timeout_secs: Option<u64>,
    /// When `Some(true)`, a completed subagent's isolated worktree is snapshotted into a durable git ref and its directory deleted.
    /// Resume restores the worktree from the ref.
    /// It is the fallback when no local `[features] subagent_worktree_snapshot` is set in config.toml; absent defaults to disabled.
    #[serde(default)]
    pub subagent_worktree_snapshot_enabled: Option<bool>,
    /// Gates `image_gen` and `/imagine`. `None` falls through env, `[features]`, then the default (on).
    #[serde(default)]
    pub image_gen_enabled: Option<bool>,
    /// Remote settings flag: optional Imagine model override for `image_gen`.
    /// When present and non-empty, `image_gen` uses this model slug (e.g. `grok-imagine-image`) instead of the default `grok-imagine-image-quality`.
    /// Absent or empty uses the default model.
    #[serde(default)]
    pub image_gen_model_override: Option<String>,
    /// Optional Imagine model override for `image_edit`. Absent or empty uses the default.
    #[serde(default)]
    pub image_edit_model_override: Option<String>,
    /// Gates the video tools and `/imagine-video`. `None` falls through env, `[features]`, then the default (on).
    #[serde(default)]
    pub video_gen_enabled: Option<bool>,
    /// When `Some(true)`, enable the process-wide image normalize cache, which shares decode, integrity-check, and re-encode work across SessionActors.
    /// Default: disabled. See `session::normalize_cache`.
    #[serde(default)]
    pub image_normalize_cache_enabled: Option<bool>,
    /// When `Some(true)`, enrich path-not-found errors with CWD reminders, "did you mean?" corrections, and similar-name suggestions.
    /// When `Some(false)` or absent, error messages are unchanged.
    #[serde(default)]
    pub path_not_found_hints: Option<bool>,
    /// Remote enable tier for the per-tip contextual hints.
    /// Each field is a soft default for one tip: `Some(false)` disables, `Some(true)` enables, absent or null uses the client default (on).
    /// User config beats this tier.
    #[serde(default)]
    pub contextual_hints: Option<ContextualHintsRemote>,
    /// Server-recommended worktree creation type.
    /// It is the fallback when no local `[cli] worktree_type` is set in config.toml.
    #[serde(default)]
    pub worktree_type: Option<String>,
    /// Grove-projected worktree strategy (`true` means grove-fuse or grove-nfs, `false` means copy).
    /// `Some(false)` is the remote kill switch. `nfs_worktree` is a deserialize alias.
    #[serde(default, alias = "nfs_worktree")]
    pub grove_worktree: Option<bool>,
    /// Server-recommended default for `restore_code` in worktree resume.
    /// It applies only when the client omits `restoreCode`.
    #[serde(default)]
    pub restore_code: Option<bool>,
    /// When `Some(true)`, Ctrl+C before the first server activity rewinds the prompt back into the input box instead of cancelling the turn.
    #[serde(default)]
    pub cancel_rewind_enabled: Option<bool>,
    /// Enables the session recap feature (`/recap` and the automatic recap on returning from away).
    /// Optional remote kill-switch; the shell defaults to on when unset (set `false` to disable).
    #[serde(default)]
    pub session_recap: Option<bool>,
    /// Enables the session search index (`/load` deep search).
    /// Optional remote kill-switch; the shell defaults to on when unset (set `false` to disable).
    #[serde(default)]
    pub session_search: Option<bool>,
    /// Enables the per-turn dashboard summary (the one-line "what happened last turn" generated at turn end).
    /// Optional remote kill-switch; the shell defaults to on when unset (set `false` to disable).
    #[serde(default)]
    pub turn_summary: Option<bool>,
    /// Enables the early-session auto-title refresh (regenerate the title from the whole conversation at a couple of early turns, then freeze).
    /// Optional remote kill-switch; when unset the shell defers to `turn_summary`, so both post-turn calls default together without being coupled.
    #[serde(default)]
    pub title_refresh: Option<bool>,
    /// Enables the `ask_user_question` tool.
    /// Optional remote kill-switch: `Some(false)` strips the tool; `Some(true)` or absent uses the shell default (on).
    #[serde(default)]
    pub ask_user_question_enabled: Option<bool>,
    /// When `Some(true)`, enable the `web_fetch` tool.
    /// When `Some(false)` or absent, the tool is not registered.
    /// Remote settings control it so the rollout can be gradual.
    #[serde(default)]
    pub web_fetch_enabled: Option<bool>,
    /// Egress proxy endpoint for the web_fetch tool.
    /// It is the fallback when no local `[toolset.web_fetch] proxy_endpoint` is set.
    #[serde(default)]
    pub web_fetch_proxy: Option<String>,
    /// Domain allowlist for the web_fetch tool.
    /// It is the fallback when no local `[toolset.web_fetch] allowed_domains` is set.
    #[serde(default)]
    pub web_fetch_allowed_domains: Option<Vec<String>>,
    /// When `Some(false)`, hide the resolved model ID in /session-info.
    #[serde(default)]
    pub show_resolved_model: Option<bool>,
    /// When `Some(true)`, enable session sharing.
    /// When `Some(false)` or absent, sharing is disabled.
    #[serde(default)]
    pub sharing_enabled: Option<bool>,
    /// Voice mode (STT dictation). The client default is on when absent.
    /// `Some(false)` is a remote kill switch; `Some(true)` forces on.
    /// `GROK_VOICE_MODE` overrides it locally. The free-tier SuperGrok upsell is a separate client tier gate.
    #[serde(default)]
    pub voice_mode_enabled: Option<bool>,
    /// Consolidated panel dock above the prompt. Off when absent.
    /// `Some(true)` from `distill_settings.dock_enabled` turns it on for the targeted cohort.
    /// `GROK_DOCK` (or the older `GROK_DOCK_V2`) overrides it locally.
    #[serde(default)]
    pub dock_enabled: Option<bool>,
    /// The terminal-native `terminal` color theme (staged rollout). Hidden when absent.
    /// `Some(true)` from `distill_settings.terminal_theme_enabled` reveals it for the targeted cohort.
    /// `GROK_TERMINAL_THEME` overrides it locally.
    #[serde(default)]
    pub terminal_theme_enabled: Option<bool>,
    /// Whether ZDR (Zero Data Retention) users are allowed to use the product.
    /// The default is `false` (blocked) during beta.
    #[serde(default)]
    pub zdr_access_enabled: Option<bool>,
    /// When `Some(true)`, the client may show the coding-data sharing upsell banner.
    /// Absent or `None` means off, so older servers and missing flags keep the banner hidden.
    #[serde(default)]
    pub privacy_notice_rollout: Option<bool>,
    /// Days after a privacy-banner dismiss before it may re-show for users who remain opted out of coding-data sharing.
    /// It comes from `distill_settings`; `None` or `0` means it never re-shows after dismiss.
    #[serde(default)]
    pub privacy_banner_reshow_days: Option<u64>,
    /// Remote settings tier of the `remember_tool_approvals` gate (whether per-tool "Always allow …" prompt options are shown).
    /// It has the lowest precedence and is typically targeted per-org. The default is `true`; `Some(false)` is a kill-switch.
    #[serde(default)]
    pub remember_tool_approvals: Option<bool>,
    /// Remote settings tier of the crash-handler install gate.
    /// It has the lowest precedence in `resolve_crash_handler_enabled`; default off. `Some(false)` is a kill-switch.
    #[serde(default)]
    pub crash_handler_enabled: Option<bool>,
    /// Whether the TUI shows agent thinking/reasoning blocks in scrollback.
    /// `None` defers to local config, env, then the default (`true`); `Some(false)` is a remote kill-switch.
    /// `resolve_show_thinking_blocks` resolves it: requirements, env, user, managed, remote, then the default (true).
    #[serde(default)]
    pub show_thinking_blocks: Option<bool>,
    /// Whether the TUI folds runs of consecutive non-destructive tool calls (reads, searches, lists) into one transcript row.
    /// `None` defers to local config, env, then the default (`true`); `Some(false)` is a remote kill-switch.
    /// `resolve_group_tool_verbs` resolves it: requirements, env, user, managed, remote, then the default (true).
    #[serde(default)]
    pub group_tool_verbs: Option<bool>,
    /// Whether the TUI shows Edit tool calls as a collapsed one-line `+N/-M` diffstat summary by default.
    /// It also merges back-to-back edits to the same file into one row (expand for the diffs).
    /// `None` defers to local config, env, then the default (`false`); `Some(false)` is a remote kill switch.
    /// `resolve_collapsed_edit_blocks` resolves it: requirements, env, user, managed, remote, then the default (false).
    /// Explicit pager.toml `[scrollback.blocks.edit]` shape keys override the flag's fold shape client-side; merging always follows the flag.
    #[serde(default)]
    pub collapsed_edit_blocks: Option<bool>,
    /// Display-refresh probe and auto-cadence. See [`DisplayRefreshSettings`].
    /// A partial object falls through per-field; `resolve_display_refresh` resolves it.
    #[serde(default)]
    pub display_refresh: Option<DisplayRefreshSettings>,
    /// Raw remote settings JSON for the `[auto_mode]` table (gate `enabled`, `prompt_type`, `classifier_model`).
    /// The shell coerces it into its typed `AutoModeConfig`, keeping this crate free of that dependency.
    /// It is the lowest-precedence layer in `resolve_auto_permission_mode_enabled` (client default on).
    #[serde(default)]
    pub auto_mode: Option<serde_json::Value>,
    /// Remote next-prompt suggestion settings, parsed by the shell.
    #[serde(default)]
    pub prompt_suggestions: Option<serde_json::Value>,
    /// Soft default permission mode (`"ask"`, `"auto"`, `"always-approve"`, or `"default"`).
    /// It is used only when no effective TOML permission key is set.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// User's subscription tier from remote settings `distill_access_gate`.
    /// E.g. "free", "premium", "supergrok", "supergrok_heavy".
    /// It is stamped on analytics events and the user profile for filtering.
    #[serde(default)]
    pub subscription_tier: Option<String>,
    #[serde(default)]
    pub gate_message: Option<String>,
    #[serde(default)]
    pub gate_url: Option<String>,
    #[serde(default)]
    pub gate_label: Option<String>,
    /// A usable `id`, non-empty `body` and positive `version` turn it on; anything else fails open.
    #[serde(default, deserialize_with = "deserialize_tolerant")]
    pub consent_gate: Option<ConsentGate>,
    /// Whether the session picker groups entries by repo name.
    /// When `None` or `Some(false)`, sessions are shown in a flat list.
    #[serde(default)]
    pub session_picker_grouped: Option<bool>,
    /// Whether the user is allowed to use Distill. Remote settings `distill_access_gate` targeting rules set it.
    /// `None` means no server response yet (the client uses its own fallback check); `Some(false)` means blocked.
    #[serde(default)]
    pub allow_access: Option<bool>,
    /// User-friendly display name for the current subscription tier
    /// (e.g. "SuperGrok", "X Premium+", "Free", "API Key"). Set by CCP
    /// from the JWT tier claim (OAuth) or credential kind (API key).
    /// Free/Invalid OAuth → `"Free"`; API keys → `"API Key"` (Mixpanel
    /// `api_key`, never free).
    #[serde(default)]
    pub subscription_tier_display: Option<String>,
    /// Whether on-demand credit usage is enabled. When `Some(false)`, the billing extension blocks on-demand cap changes.
    #[serde(default)]
    pub on_demand_enabled: Option<bool>,
    /// When set to a non-empty URL, the pager's `/usage` command shows a link to that URL instead of fetching billing data from the backend.
    /// The remote settings `distill_usage_redirect_url` feature flag controls it (target it at personal-team users).
    /// `None` or empty keeps the default of fetching usage from the backend.
    #[serde(default)]
    pub usage_billing_redirect_url: Option<String>,
    /// Enable the shell command suggestion pipeline remotely.
    #[serde(default)]
    pub suggestions_enabled: Option<bool>,
    /// Enable AI-powered shell command suggestions remotely.
    #[serde(default)]
    pub suggestions_ai_enabled: Option<bool>,
    /// Global auto-compact threshold percent (0-100) from remote settings `distill_settings`.
    /// A per-model override on `ModelInfo` (`distill_models`) takes precedence; user config and env var further override per the resolver chain.
    #[serde(default)]
    pub auto_compact_threshold_percent: Option<u8>,
    /// Max subagent nesting depth (`distill_settings.subagents_max_depth`).
    #[serde(default)]
    pub subagents_max_depth: Option<u32>,
    #[serde(default)]
    pub subagents_max_concurrent: Option<u32>,
    #[serde(default)]
    pub subagents_sampling_limit: Option<u32>,
    /// `"queue"` or `"fail"`.
    #[serde(default)]
    pub subagents_limit_behavior: Option<String>,
    #[serde(default)]
    pub workflow_max_concurrent_agents: Option<u32>,
    /// Max parallel `image_gen` and `image_edit` tool calls in one model step (`distill_settings.max_parallel_image_gen_calls`).
    #[serde(default)]
    pub max_parallel_image_gen_calls: Option<u32>,
    /// Max parallel video-gen tool calls in one model step (`distill_settings.max_parallel_video_gen_calls`).
    #[serde(default)]
    pub max_parallel_video_gen_calls: Option<u32>,
    /// Global system-prompt identity label. A per-model override wins; see `resolve_system_prompt_label`.
    #[serde(default)]
    pub system_prompt_label: Option<String>,
    /// Global per-compaction wall-clock budget (seconds) from remote settings; `0` disables.
    /// Env (`GROK_COMPACTION_WALL_CLOCK_SECS`) overrides it. `resolve_compaction_wall_clock_budget_secs` resolves it.
    #[serde(default)]
    pub compaction_wall_clock_budget_secs: Option<u64>,
    /// Compaction mode (`summary`, `transcript`, or `segments`) from remote settings.
    /// Env (`GROK_COMPACTION_MODE`) and user config override it.
    #[serde(default)]
    pub compaction_mode: Option<String>,
    /// Segments verbatim detail (`none`, `minimal`, `balanced`, or `verbose`) from remote settings.
    /// Env (`GROK_COMPACTION_DETAIL`) and config override it.
    #[serde(default)]
    pub compaction_detail: Option<String>,
    /// remote settings verbatim-input flag; env (`GROK_COMPACTION_VERBATIM_INPUT`) and config override it. `None` = default (true).
    #[serde(default)]
    pub compaction_verbatim_input: Option<bool>,
    #[serde(default)]
    pub compaction_tool_choice: Option<String>,
    /// Remote settings denylist of optional imagine tools to disable (e.g. `["image_edit"]`).
    /// When a tool is listed it is removed from the toolset and local env/config can't re-enable it.
    /// Absent or not listed means each tool keeps its own default.
    /// See `Config::resolve_image_edit`.
    #[serde(default)]
    pub imagine_tools_disabled: Option<Vec<String>>,
    /// Remote settings gate for the `grok workspace` CLI command (Computer Hub workspace exposure).
    /// It comes from `distill_settings.workspace_command_enabled`.
    /// `Some(true)` enables it; `None` or `Some(false)` (the default) keep it off.
    #[serde(default)]
    pub workspace_command_enabled: Option<bool>,
    #[serde(default)]
    pub workspace_dashboard_enabled: Option<bool>,
    /// Soft default for `keep_text_selection` (`"flash"`, `"hold"`, or `"word_select"`), from `distill_settings.keep_text_selection_default`.
    /// It applies only when the user has set no local text-selection preference; an explicit local `keep_text_selection` always wins.
    /// An absent or unrecognized value keeps the client default (`flash`).
    /// Set it remotely to stage a new default to a segment, cut everyone over, or revert it for a customer.
    #[serde(default)]
    pub keep_text_selection_default: Option<String>,
    /// Master switch for jemalloc heap sampling and threshold dumps.
    /// `Some(true)` enables, `Some(false)` is a kill-switch, `None` uses the client default (off).
    #[serde(default)]
    pub jemalloc_heap_profile_enabled: Option<bool>,
    /// Resident-byte thresholds (e.g. 2G/5G/10G as byte counts).
    /// `None` and `[]` are distinct on the wire.
    #[serde(default)]
    pub jemalloc_heap_profile_thresholds_bytes: Option<Vec<u64>>,
    /// Stats poll interval in seconds when set.
    #[serde(default)]
    pub jemalloc_heap_profile_poll_interval_secs: Option<u64>,
}
/// A request body encoding advertised in [`RemoteSettings::accept_request_encodings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteRequestEncoding {
    Zstd,
    /// Forward-compat: an encoding this build does not know.
    #[serde(other)]
    Unknown,
}
impl RemoteSettings {
    /// Denylist check for an optional imagine tool.
    /// Returns `true` when the server sent `imagine_tools_disabled` and it contains `tool` (force-off).
    /// Otherwise `false`, deferring to the tool's own default.
    pub fn imagine_tool_disabled(&self, tool: &str) -> bool {
        self.imagine_tools_disabled
            .as_ref()
            .is_some_and(|list| list.iter().any(|t| t == tool))
    }
}
/// Remote enable tier for the per-tip contextual hints (mirrors the client's `[ui.contextual_hints]` shape).
/// Each field is a soft default for one tip; `None` defers to the client default (on).
/// Every field is `#[serde(default)]` so a partial object from remote settings never fails the whole `RemoteSettings` parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContextualHintsRemote {
    /// Undo tip (Ctrl+Z after a substantial draft wipe).
    #[serde(default)]
    pub undo: Option<bool>,
    /// Plan-mode nudge (typing a planning keyword).
    #[serde(default)]
    pub plan_mode: Option<bool>,
    /// Clipboard-image input tip.
    #[serde(default)]
    pub image_input: Option<bool>,
    /// Send-now tip after queuing a mid-turn follow-up.
    #[serde(default)]
    pub send_now: Option<bool>,
    /// Small-screen tip (`/compact-mode` hint on smallish terminals).
    #[serde(default)]
    pub small_screen: Option<bool>,
    /// Word-select tip after a double-click fold or nav (helps users find the setting).
    #[serde(default)]
    pub word_select: Option<bool>,
    /// Export/copy tip after three nearby drag-copies.
    #[serde(default)]
    pub export_copy: Option<bool>,
    /// SSH wrap session-load tip (recommend `grok wrap ssh` for remote sessions).
    #[serde(default)]
    pub ssh_wrap: Option<bool>,
}
/// Tolerant deserializer for `Option<Vec<RemoteAnnouncement>>`.
/// Parses as Vec<Value>, tries each as RemoteAnnouncement, and drops failures with a warning.
/// One bad item cannot poison the whole RemoteSettings.
fn deserialize_tolerant_announcements<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<RemoteAnnouncement>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match serde_json::from_value::<RemoteAnnouncement>(item) {
                    Ok(a) => out.push(a),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "remote settings announcements: dropped malformed item"
                        );
                    }
                }
            }
            Ok(Some(out))
        }
        Some(_) => Ok(None),
    }
}
/// Parse one JSON value as a [`GoalRoleModel`], returning `None` (with a `tracing::warn!`) instead of erroring when the value is malformed.
/// The tolerant deserializers for the single-pair role fields and the skeptic pool share it.
/// All three goal-role-model fields drop bad remote payloads rather than failing the whole `RemoteSettings` parse.
fn parse_goal_role_model_tolerant(value: serde_json::Value) -> Option<GoalRoleModel> {
    match serde_json::from_value::<GoalRoleModel>(value) {
        Ok(model) => Some(model),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "remote settings goal role model: dropped malformed value"
            );
            None
        }
    }
}
/// Tolerant deserializer for `Option<GoalRoleModel>` (the single-pair role fields).
/// Parses as `Option<Value>`; a malformed value or an explicit `null` maps to `None` via [`parse_goal_role_model_tolerant`] rather than erroring.
/// One bad remote payload cannot fail the whole `RemoteSettings` parse.
fn deserialize_tolerant_goal_role_model<'de, D>(
    deserializer: D,
) -> Result<Option<GoalRoleModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(match opt {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => parse_goal_role_model_tolerant(value),
    })
}
/// Tolerant deserializer for `Vec<GoalRoleModel>` (the skeptic pool).
/// Parses as `Option<Value>`; a non-array, null, or absent value yields an empty pool.
/// Within an array each malformed entry is dropped (via [`parse_goal_role_model_tolerant`]) instead of discarding the whole pool.
/// Survivor order is preserved: the skeptic round-robin assignment (`expand_skeptic_assignment`) depends on pool order.
fn deserialize_tolerant_goal_skeptic_models<'de, D>(
    deserializer: D,
) -> Result<Vec<GoalRoleModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    match opt {
        Some(serde_json::Value::Array(arr)) => Ok(arr
            .into_iter()
            .filter_map(parse_goal_role_model_tolerant)
            .collect()),
        _ => Ok(Vec::new()),
    }
}
/// A model and the harness whose system prompt and toolset flavor that model must run against.
/// The pair is the atomic configurable unit because a model is only guaranteed to work with a compatible harness (cursor vs distill).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRoleModel {
    /// Model id, e.g. "grok-4". It resolves against available models at spawn time; unknown or unauthorized fails open to the current model.
    pub model: String,
    /// Harness `agent_type` (e.g. "cursor", "distill-plan") whose `AgentDefinition` decides the role subagent's harness flavor.
    /// The flavor (system prompt and cursor-vs-distill toolset) applies regardless of the session or parent agent.
    /// It is resolved by name (project/plugin/builtin lookup, then re-flavored by the subagent toolset resolver).
    /// The main session's env/ACP/strict-harness precedence chain plays no part.
    /// It is not a subagent type: the role always spawns `general-purpose`, so the harness only re-flavors that toolset.
    /// An `agent_type` that doesn't resolve, or whose role toolset can't satisfy the role, fails open to the session model and harness before commit.
    /// One that resolves to a strict harness whose flavor the subagent system can't represent (e.g. `codex`) fails open the same way.
    pub agent_type: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worktree_auto_gc_partial_object_and_round_trip() {
        let json = r#"{"worktree_auto_gc":{"enabled":false}}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        let agc = s.worktree_auto_gc.as_ref().expect("present");
        assert_eq!(agc.enabled, Some(false));
        assert_eq!(agc.max_age_secs, None);
        assert_eq!(agc.min_interval_secs, None);
        assert_eq!(agc.dry_run, None);
        assert_eq!(agc.include_orphan_snapshots, None);
        assert_eq!(agc.max_age_by_kind, None);
        let full = r#"{
            "worktree_auto_gc": {
                "enabled": true,
                "max_age_secs": 604800,
                "min_interval_secs": 21600,
                "dry_run": true,
                "include_orphan_snapshots": false,
                "max_age_by_kind": {
                    "session": 604800,
                    "subagent": 86400,
                    "manual": "never"
                }
            }
        }"#;
        let s: RemoteSettings = serde_json::from_str(full).unwrap();
        let agc = s.worktree_auto_gc.clone().unwrap();
        let mut kind_map = std::collections::BTreeMap::new();
        kind_map.insert("session".into(), WorktreeKindMaxAge::Secs(604800));
        kind_map.insert("subagent".into(), WorktreeKindMaxAge::Secs(86400));
        kind_map.insert("manual".into(), WorktreeKindMaxAge::Never);
        assert_eq!(
            agc,
            WorktreeAutoGcSettings {
                enabled: Some(true),
                max_age_secs: Some(604800),
                min_interval_secs: Some(21600),
                dry_run: Some(true),
                include_orphan_snapshots: Some(false),
                max_age_by_kind: Some(kind_map),
                include_rebuild: None,
                rebuild_min_interval_secs: None,
            }
        );
        let out = serde_json::to_string(&s).unwrap();
        let s2: RemoteSettings = serde_json::from_str(&out).unwrap();
        assert_eq!(s2.worktree_auto_gc, s.worktree_auto_gc);
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.worktree_auto_gc, None);
        let extra = r#"{"worktree_auto_gc":{"enabled":true,"future_knob":1}}"#;
        let s: RemoteSettings = serde_json::from_str(extra).unwrap();
        assert_eq!(s.worktree_auto_gc.unwrap().enabled, Some(true));
        let partial_bad = r#"{"worktree_auto_gc":{"enabled":false,"max_age_secs":"nope"}}"#;
        let s: RemoteSettings = serde_json::from_str(partial_bad).unwrap();
        let agc = s.worktree_auto_gc.as_ref().expect("object still present");
        assert_eq!(agc.enabled, Some(false));
        assert_eq!(agc.max_age_secs, None);
        let kind_partial = r#"{
            "worktree_auto_gc": {
                "max_age_by_kind": {
                    "subagent": 86400,
                    "session": {"nested": true},
                    "manual": "never"
                }
            }
        }"#;
        let s: RemoteSettings = serde_json::from_str(kind_partial).unwrap();
        let map = s
            .worktree_auto_gc
            .as_ref()
            .and_then(|a| a.max_age_by_kind.as_ref())
            .expect("map present");
        assert_eq!(map.get("subagent"), Some(&WorktreeKindMaxAge::Secs(86400)));
        assert_eq!(map.get("manual"), Some(&WorktreeKindMaxAge::Never));
        assert!(!map.contains_key("session"));
        let null_never =
            r#"{"worktree_auto_gc":{"max_age_by_kind":{"manual":null,"pool":172800}}}"#;
        let s: RemoteSettings = serde_json::from_str(null_never).unwrap();
        let map = s.worktree_auto_gc.unwrap().max_age_by_kind.unwrap();
        assert_eq!(map.get("manual"), Some(&WorktreeKindMaxAge::Never));
        assert_eq!(map.get("pool"), Some(&WorktreeKindMaxAge::Secs(172800)));
        assert_eq!(
            serde_json::to_value(&WorktreeKindMaxAge::Never).unwrap(),
            serde_json::Value::String("never".into())
        );
        let nested_bad = r#"{"leader_mode":true,"worktree_auto_gc":"not-an-object"}"#;
        let s: RemoteSettings = serde_json::from_str(nested_bad).unwrap();
        assert_eq!(s.leader_mode, Some(true));
        assert_eq!(s.worktree_auto_gc, None);
    }
    #[test]
    fn remote_settings_turn_transient_retry_malformed_value_does_not_poison_siblings() {
        let s: RemoteSettings =
            serde_json::from_str(r#"{"turn_transient_retry": "false", "leader_mode": true}"#)
                .unwrap();
        assert_eq!(
            s.turn_transient_retry, None,
            "malformed value drops to None"
        );
        assert_eq!(s.leader_mode, Some(true), "siblings survive");
    }
    #[test]
    fn tolerant_bool_swallows_array_and_object_shapes() {
        for bad in [
            r#"{"turn_transient_retry": [1,2], "leader_mode": true}"#,
            r#"{"turn_transient_retry": {"a": 1}, "leader_mode": true}"#,
        ] {
            let s: RemoteSettings = serde_json::from_str(bad).unwrap();
            assert_eq!(s.turn_transient_retry, None);
            assert_eq!(s.leader_mode, Some(true), "siblings survive: {bad}");
        }
    }
    #[test]
    fn remote_settings_turn_transient_retry_round_trip_and_default_absent() {
        let s: RemoteSettings = serde_json::from_str(r#"{"turn_transient_retry": false}"#).unwrap();
        assert_eq!(s.turn_transient_retry, Some(false));
        let round_trip: RemoteSettings =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round_trip.turn_transient_retry, Some(false));
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.turn_transient_retry, None);
    }
    #[test]
    fn remote_settings_uncharged_401_park_round_trip_and_default_absent() {
        let s: RemoteSettings = serde_json::from_str(r#"{"uncharged_401_park": false}"#).unwrap();
        assert_eq!(s.uncharged_401_park, Some(false));
        let round_trip: RemoteSettings =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round_trip.uncharged_401_park, Some(false));
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.uncharged_401_park, None);
    }
    #[test]
    fn remote_settings_uncharged_401_park_malformed_value_does_not_poison_siblings() {
        for bad in [
            r#"{"uncharged_401_park": "false", "leader_mode": true}"#,
            r#"{"uncharged_401_park": [1], "leader_mode": true}"#,
        ] {
            let s: RemoteSettings = serde_json::from_str(bad).unwrap();
            assert_eq!(s.uncharged_401_park, None, "malformed value drops: {bad}");
            assert_eq!(s.leader_mode, Some(true), "siblings survive: {bad}");
        }
    }
    #[test]
    fn remote_settings_vendor_sessions_round_trip_and_default_absent() {
        let session_flags = |settings: &RemoteSettings| {
            (
                settings.cursor_sessions_enabled,
                settings.claude_sessions_enabled,
                settings.codex_sessions_enabled,
            )
        };
        let json = r#"{
            "cursor_sessions_enabled": true,
            "claude_sessions_enabled": false,
            "codex_sessions_enabled": true
        }"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            session_flags(&settings),
            (Some(true), Some(false), Some(true))
        );
        let serialized = serde_json::to_string(&settings).unwrap();
        let round_trip: RemoteSettings = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            session_flags(&round_trip),
            (Some(true), Some(false), Some(true))
        );
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(session_flags(&absent), (None, None, None));
    }
    #[test]
    fn remote_settings_image_description_model_round_trip() {
        let json = r#"{"image_description_model": "grok-4.6"}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.image_description_model.as_deref(), Some("grok-4.6"));
        let out = serde_json::to_string(&s).unwrap();
        let s2: RemoteSettings = serde_json::from_str(&out).unwrap();
        assert_eq!(s2.image_description_model, s.image_description_model);
    }
    #[test]
    fn remote_settings_prompt_suggestion_model_round_trip() {
        let json = r#"{"prompt_suggestion_model": "grok-4.6"}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.prompt_suggestion_model.as_deref(), Some("grok-4.6"));
        let out = serde_json::to_string(&s).unwrap();
        let s2: RemoteSettings = serde_json::from_str(&out).unwrap();
        assert_eq!(s2.prompt_suggestion_model, s.prompt_suggestion_model);
        let s3: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s3.prompt_suggestion_model, None);
    }
    #[test]
    fn remote_settings_announcements_absent() {
        let json = r#"{}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.announcements, None);
    }
    #[test]
    fn remote_settings_announcements_populated() {
        let json = r#"{"announcements": [{"id": "a", "message": "m"}]}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            settings.announcements,
            Some(vec![RemoteAnnouncement {
                id: Some("a".to_string()),
                message: Some("m".to_string()),
                severity: None,
                title: None,
                cta: None,
                updated_at: None,
                expires_at: None,
                dismissible: None,
                persistent: None,
            }])
        );
    }
    #[test]
    fn remote_settings_announcements_one_bad_item_does_not_poison() {
        let json = r#"{
            "announcements": [
                {"id": "good", "message": "ok"},
                {"id": 999, "message": "bad-id-type"}
            ]
        }"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            settings.announcements,
            Some(vec![RemoteAnnouncement {
                id: Some("good".to_string()),
                message: Some("ok".to_string()),
                severity: None,
                title: None,
                cta: None,
                updated_at: None,
                expires_at: None,
                dismissible: None,
                persistent: None,
            }])
        );
    }
    #[test]
    fn remote_settings_consent_gate_round_trip() {
        let json = r#"{
            "consent_gate": {
                "id": "tos-2026-08",
                "version": 3,
                "title": "Updated terms",
                "body": "Review our [Terms of Service](https://x.ai/legal/tos) before continuing.",
                "accept_label": "Accept and continue"
            }
        }"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        let gate = settings.consent_gate.expect("gate present");
        assert_eq!(gate.id, "tos-2026-08");
        assert_eq!(gate.version, Some(3));
        assert!(
            gate.body
                .as_deref()
                .is_some_and(|b| b.contains("https://x.ai/legal/tos"))
        );
    }
    /// A poisoned response would leave `zdr_access_enabled` false and hard-block ZDR users.
    #[test]
    fn remote_settings_malformed_consent_gate_does_not_poison() {
        for gate in [
            r#"{"id": "tos", "version": "not-a-number"}"#,
            r#"{"version": 3, "body": "Review our terms."}"#,
        ] {
            let json = format!(r#"{{"consent_gate": {gate}, "tips": ["still parsed"]}}"#);
            let settings: RemoteSettings = serde_json::from_str(&json).unwrap();
            assert!(settings.consent_gate.is_none(), "{gate} must be dropped");
            assert_eq!(settings.tips, Some(vec!["still parsed".to_string()]));
        }
    }
    #[test]
    fn remote_settings_goal_planner_model_round_trip() {
        let json =
            r#"{"goal_planner_model": {"model": "grok-4", "agent_type": "general-purpose"}}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_planner_model,
            Some(GoalRoleModel {
                model: "grok-4".to_string(),
                agent_type: "general-purpose".to_string(),
            })
        );
        let out = serde_json::to_string(&s).unwrap();
        let s2: RemoteSettings = serde_json::from_str(&out).unwrap();
        assert_eq!(s2.goal_planner_model, s.goal_planner_model);
    }
    #[test]
    fn remote_settings_goal_skeptic_models_fully_valid_pool_round_trips() {
        let json = r#"{"goal_skeptic_models": [
            {"model": "grok-4", "agent_type": "general-purpose"},
            {"model": "grok-3", "agent_type": "cursor"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_skeptic_models,
            vec![
                GoalRoleModel {
                    model: "grok-4".to_string(),
                    agent_type: "general-purpose".to_string(),
                },
                GoalRoleModel {
                    model: "grok-3".to_string(),
                    agent_type: "cursor".to_string(),
                },
            ]
        );
        let out = serde_json::to_string(&s).unwrap();
        let s2: RemoteSettings = serde_json::from_str(&out).unwrap();
        assert_eq!(s2.goal_skeptic_models, s.goal_skeptic_models);
    }
    #[test]
    fn remote_settings_goal_skeptic_models_one_bad_item_does_not_poison_pool() {
        let json = r#"{"goal_skeptic_models": [
            {"model": "grok-4", "agent_type": "general-purpose"},
            {"model": "grok-broken"},
            {"model": "grok-3", "agent_type": "cursor"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_skeptic_models,
            vec![
                GoalRoleModel {
                    model: "grok-4".to_string(),
                    agent_type: "general-purpose".to_string(),
                },
                GoalRoleModel {
                    model: "grok-3".to_string(),
                    agent_type: "cursor".to_string(),
                },
            ]
        );
    }
    #[test]
    fn remote_settings_goal_skeptic_models_all_entries_bad_yields_empty() {
        let json = r#"{"goal_skeptic_models": [
            {"model": "only-model"},
            {"agent_type": "only-agent-type"},
            "scalar-not-an-object",
            42
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert!(s.goal_skeptic_models.is_empty());
    }
    #[test]
    fn remote_settings_goal_skeptic_models_non_array_yields_empty() {
        for json in [
            r#"{"goal_skeptic_models": {"model": "x", "agent_type": "y"}}"#,
            r#"{"goal_skeptic_models": "not-an-array"}"#,
            r#"{"goal_skeptic_models": 7}"#,
        ] {
            let s: RemoteSettings = serde_json::from_str(json).unwrap();
            assert!(
                s.goal_skeptic_models.is_empty(),
                "non-array pool must yield empty for {json}"
            );
        }
    }
    #[test]
    fn remote_settings_goal_skeptic_models_missing_model_entry_dropped() {
        let json = r#"{"goal_skeptic_models": [
            {"agent_type": "general-purpose"},
            {"model": "grok-3", "agent_type": "cursor"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_skeptic_models,
            vec![GoalRoleModel {
                model: "grok-3".to_string(),
                agent_type: "cursor".to_string(),
            }]
        );
    }
    #[test]
    fn remote_settings_goal_skeptic_models_wrong_typed_scalar_dropped() {
        let json = r#"{"goal_skeptic_models": [
            {"model": 123, "agent_type": "general-purpose"},
            {"model": "grok-3", "agent_type": ["cursor"]},
            {"model": "grok-4", "agent_type": "general-purpose"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_skeptic_models,
            vec![GoalRoleModel {
                model: "grok-4".to_string(),
                agent_type: "general-purpose".to_string(),
            }]
        );
    }
    #[test]
    fn remote_settings_goal_skeptic_models_extra_unknown_fields_kept() {
        let json = r#"{"goal_skeptic_models": [
            {"model": "grok-4", "agent_type": "general-purpose", "reasoning_effort": "high"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_skeptic_models,
            vec![GoalRoleModel {
                model: "grok-4".to_string(),
                agent_type: "general-purpose".to_string(),
            }]
        );
    }
    #[test]
    fn remote_settings_goal_skeptic_models_survivor_order_preserved() {
        let json = r#"{"goal_skeptic_models": [
            {"model": "first", "agent_type": "general-purpose"},
            {"model": "bad"},
            {"model": "second", "agent_type": "cursor"},
            "garbage",
            {"model": "third", "agent_type": "general-purpose"}
        ]}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        let order: Vec<&str> = s
            .goal_skeptic_models
            .iter()
            .map(|m| m.model.as_str())
            .collect();
        assert_eq!(order, vec!["first", "second", "third"]);
    }
    #[test]
    fn remote_settings_goal_planner_model_malformed_yields_none() {
        for json in [
            r#"{"goal_planner_model": {"model": "only-model"}}"#,
            r#"{"goal_planner_model": {"agent_type": "only-agent-type"}}"#,
            r#"{"goal_planner_model": {"model": 1, "agent_type": "x"}}"#,
            r#"{"goal_planner_model": "scalar"}"#,
            r#"{"goal_planner_model": null}"#,
        ] {
            let s: RemoteSettings = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("must not hard-error for {json}: {e}"));
            assert_eq!(s.goal_planner_model, None, "for {json}");
        }
    }
    #[test]
    fn remote_settings_goal_role_models_malformed_pair_does_not_drop_other_fields() {
        let json = r#"{
            "goal_planner_model": {"model": "broken"},
            "goal_strategist_model": {"model": "grok-4.5", "agent_type": "cursor"},
            "default_model": "grok-4"
        }"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.goal_planner_model, None);
        assert_eq!(
            s.goal_strategist_model,
            Some(GoalRoleModel {
                model: "grok-4.5".to_string(),
                agent_type: "cursor".to_string(),
            })
        );
        assert_eq!(s.default_model.as_deref(), Some("grok-4"));
    }
    #[test]
    fn remote_settings_goal_role_model_extra_unknown_fields_kept_single_pair() {
        let json = r#"{"goal_planner_model": {"model": "grok-4", "agent_type": "general-purpose", "future": true}}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.goal_planner_model,
            Some(GoalRoleModel {
                model: "grok-4".to_string(),
                agent_type: "general-purpose".to_string(),
            })
        );
    }
    #[test]
    fn remote_settings_inference_idle_timeout_present() {
        let json = r#"{"inference_idle_timeout_secs": 180}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.inference_idle_timeout_secs, Some(180));
    }
    #[test]
    fn remote_settings_initial_injection_deserialize_present() {
        let json = r#"{"memory_initial_injection_enabled": false, "memory_initial_injection_min_score": 0.66}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.memory_initial_injection_enabled, Some(false));
        assert_eq!(settings.memory_initial_injection_min_score, Some(0.66));
    }
    #[test]
    fn remote_settings_memory_v2_round_trips_and_defaults_absent() {
        for expected in [true, false] {
            let settings: RemoteSettings = serde_json::from_value(serde_json::json!({
                "memory_v2": {
                    "enabled": expected,
                    "capture_status_enabled": expected,
                },
            }))
            .unwrap();
            let memory_v2 = settings.memory_v2.as_ref().unwrap();
            assert_eq!(memory_v2.enabled, Some(expected));
            assert_eq!(memory_v2.capture_status_enabled, Some(expected));
            let round_trip: RemoteSettings =
                serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
            let memory_v2 = round_trip.memory_v2.as_ref().unwrap();
            assert_eq!(memory_v2.enabled, Some(expected));
            assert_eq!(memory_v2.capture_status_enabled, Some(expected));
        }
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.memory_v2, None);
    }
    #[test]
    fn remote_settings_initial_injection_deserialize_absent() {
        let json = r#"{"memory_enabled": true}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.memory_initial_injection_enabled, None);
        assert_eq!(settings.memory_initial_injection_min_score, None);
    }
    #[test]
    fn remote_settings_inference_idle_timeout_absent() {
        let json = r#"{"memory_enabled": true}"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.inference_idle_timeout_secs, None);
    }
    #[test]
    fn remote_settings_unknown_fields_tolerated() {
        let json = r#"{
            "inference_idle_timeout_secs": 120,
            "future_remote_field": 42,
            "verification_staleness_enabled": true
        }"#;
        let settings: RemoteSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.inference_idle_timeout_secs, Some(120));
    }
    #[test]
    fn remote_settings_display_refresh_present_partial() {
        let json = r#"{"display_refresh": {"auto_cadence_enabled": true, "floor_ms": 7}}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        let dr = s.display_refresh.expect("display_refresh present");
        assert_eq!(dr.auto_cadence_enabled, Some(true));
        assert_eq!(dr.floor_ms, Some(7));
        assert_eq!(dr.probe_enabled, None);
        assert_eq!(dr.ceiling_ms, None);
        assert_eq!(dr.min_hz, None);
        assert_eq!(dr.max_hz, None);
    }
    #[test]
    fn remote_settings_display_refresh_unknown_keys_preserved() {
        let json =
            r#"{"display_refresh": {"probe_enabled": true, "future_knob": 42, "floor_ms": "bad"}}"#;
        let s: RemoteSettings = serde_json::from_str(json).unwrap();
        let dr = s.display_refresh.expect("display_refresh present");
        assert_eq!(dr.probe_enabled, Some(true));
        assert_eq!(dr.floor_ms, None, "wrong-typed floor_ms ignored");
        assert_eq!(dr.extra.get("future_knob"), Some(&serde_json::json!(42)));
        let out = serde_json::to_value(&dr).unwrap();
        assert_eq!(out.get("future_knob"), Some(&serde_json::json!(42)));
        assert_eq!(out.get("probe_enabled"), Some(&serde_json::json!(true)));
    }
    #[test]
    fn remote_settings_accept_request_encodings_unknown_tolerated_and_default_empty() {
        let s: RemoteSettings =
            serde_json::from_str(r#"{"accept_request_encodings": ["zstd", "br"]}"#).unwrap();
        assert_eq!(
            vec![RemoteRequestEncoding::Zstd, RemoteRequestEncoding::Unknown],
            s.accept_request_encodings
        );
        let absent: RemoteSettings = serde_json::from_str("{}").unwrap();
        assert!(absent.accept_request_encodings.is_empty());
    }
    type JemallocFields<'a> = (Option<bool>, Option<&'a [u64]>, Option<u64>);
    fn jemalloc_fields(s: &RemoteSettings) -> JemallocFields<'_> {
        (
            s.jemalloc_heap_profile_enabled,
            s.jemalloc_heap_profile_thresholds_bytes.as_deref(),
            s.jemalloc_heap_profile_poll_interval_secs,
        )
    }
    fn parse_remote(json: &str) -> RemoteSettings {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("parse failed for {json}: {e}"))
    }
    fn round_trip_remote(s: &RemoteSettings) -> RemoteSettings {
        let out = serde_json::to_string(s).unwrap();
        parse_remote(&out)
    }
    fn assert_jemalloc_round_trip(json: &str, expected: JemallocFields<'_>) {
        let s = parse_remote(json);
        assert_eq!(jemalloc_fields(&s), expected);
        assert_eq!(jemalloc_fields(&round_trip_remote(&s)), expected);
    }
    fn assert_remote_parse_err(json: &str) {
        assert!(
            serde_json::from_str::<RemoteSettings>(json).is_err(),
            "expected parse error for {json}"
        );
    }
    #[test]
    fn remote_settings_jemalloc_heap_profile_thresholds_populated_and_empty_round_trip() {
        assert_jemalloc_round_trip(
            r#"{
                "jemalloc_heap_profile_thresholds_bytes": [2147483648, 5368709120, 10737418240]
            }"#,
            (
                None,
                Some(&[2_147_483_648, 5_368_709_120, 10_737_418_240]),
                None,
            ),
        );
        assert_jemalloc_round_trip(
            r#"{"jemalloc_heap_profile_thresholds_bytes": []}"#,
            (None, Some(&[]), None),
        );
        assert_jemalloc_round_trip(
            r#"{
                "jemalloc_heap_profile_enabled": true,
                "jemalloc_heap_profile_thresholds_bytes": []
            }"#,
            (Some(true), Some(&[]), None),
        );
    }
    #[test]
    fn remote_settings_jemalloc_heap_profile_malformed_fails_whole_parse() {
        for json in [
            r#"{"jemalloc_heap_profile_enabled": "yes"}"#,
            r#"{"jemalloc_heap_profile_enabled": 1}"#,
            r#"{"jemalloc_heap_profile_enabled": []}"#,
            r#"{"jemalloc_heap_profile_enabled": {}}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": "2G"}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": {"bytes": 1}}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": [true]}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": ["2G"]}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": [-1]}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": [null]}"#,
            r#"{"jemalloc_heap_profile_thresholds_bytes": [1, null, 2]}"#,
            r#"{"jemalloc_heap_profile_poll_interval_secs": "30"}"#,
            r#"{"jemalloc_heap_profile_poll_interval_secs": true}"#,
            r#"{"jemalloc_heap_profile_poll_interval_secs": -1}"#,
            r#"{
                "jemalloc_heap_profile_enabled": true,
                "jemalloc_heap_profile_thresholds_bytes": "bad",
                "workspace_command_enabled": true
            }"#,
        ] {
            assert_remote_parse_err(json);
        }
    }
}
