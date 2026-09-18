use crate::models;
use serde::{Deserialize, Serialize};
use xai_grok_sampler::SamplerConfig;
use xai_grok_tools::implementations::grok_build;
use xai_grok_tools::registry::types::ToolConfig;

/// The tool-server binary defaults to a 5-minute foreground ceiling (`DEFAULT_MAX_TIMEOUT_MS`).
/// Production opts *up* to 10h by sending this explicitly, overridable via config.toml.
/// This bounds only foreground commands; background tasks are always unbounded.
pub const PRODUCTION_MAX_TIMEOUT_SECS: f64 = 36_000.0; // 10 hours

/// User configurable settings for the built-in bash tool (`[toolset.bash]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BashToolConfig {
    pub timeout_secs: Option<f64>,
    /// Foreground ceiling for model-provided command timeouts (seconds).
    /// When `None`, `to_bash_params_json` emits the production default ([`PRODUCTION_MAX_TIMEOUT_SECS`], 10h).
    pub max_timeout_secs: Option<f64>,
    pub output_byte_limit: Option<usize>,
    pub cmd_prefix: Option<String>,
    /// Whether to auto-background a command when it times out (default: `true`).
    pub auto_background_on_timeout: Option<bool>,
    /// How long a command may block the foreground before auto-backgrounding, in milliseconds, when `auto_background_on_timeout` is on.
    /// `None` uses the server default of 15s.
    /// `Some(0)` disables the short budget, so auto-backgrounding happens only at the model/default timeout.
    pub foreground_block_budget_ms: Option<u64>,
    /// Whether to allow a background `&` operator in foreground commands (default: `true`).
    /// Resolution: config.toml (this) > remote settings > `true`.
    pub allow_background_operator: Option<bool>,
    pub login_shell_capture: Option<bool>,
}

impl BashToolConfig {
    /// [`Self::to_bash_params_json`] with the fallbacks read from the remote settings snapshot.
    /// The top-level session and subagent/workflow children both resolve through here so their bash limits cannot drift.
    pub(crate) fn to_bash_params_json_with_remote(
        &self,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) -> serde_json::Map<String, serde_json::Value> {
        self.to_bash_params_json(
            remote.and_then(|r| r.auto_background_on_timeout),
            remote.and_then(|r| r.allow_background_operator),
        )
    }

    /// `remote_auto_bg` is the remote settings fallback for `auto_background_on_timeout`.
    /// Resolution: local config.toml > remote fallback > `true`.
    pub(crate) fn to_bash_params_json(
        &self,
        remote_auto_bg: Option<bool>,
        remote_allow_background_operator: Option<bool>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        if let Some(t) = self.timeout_secs {
            map.insert("timeout_secs".into(), t.into());
        }
        // Production grok-build opts up to 10h by sending it explicitly (overridable via config.toml)
        let max_timeout_secs = self.max_timeout_secs.unwrap_or(PRODUCTION_MAX_TIMEOUT_SECS);
        map.insert("max_timeout_secs".into(), max_timeout_secs.into());
        if let Some(limit) = self.output_byte_limit {
            map.insert("output_byte_limit".into(), limit.into());
        }
        if let Some(ref p) = self.cmd_prefix {
            map.insert("cmd_prefix".into(), p.clone().into());
        }
        let auto_bg = self
            .auto_background_on_timeout
            .or(remote_auto_bg)
            .unwrap_or(true);
        map.insert("auto_background_on_timeout".into(), auto_bg.into());
        if let Some(ms) = self.foreground_block_budget_ms {
            map.insert("foreground_block_budget_ms".into(), ms.into());
        }
        let allow_bg_op = self
            .allow_background_operator
            .or(remote_allow_background_operator)
            .unwrap_or(true);
        map.insert("allow_background_operator".into(), allow_bg_op.into());
        map
    }
}

/// User configurable settings for the ask_user_question tool (`[toolset.ask_user_question]`). Consumed by `crate::util::config::resolve_ask_user_question_params_from_disk`, which reads the raw config layers directly.
/// That keeps the documented precedence (requirements > env > user > managed > remote). This struct exists so the keys are recognized in `config.toml` and round-trip through `AgentConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AskUserQuestionToolConfig {
    /// Whether the questionnaire timeout is enabled (default: `true`).
    /// `false` waits forever for answers.
    pub timeout_enabled: Option<bool>,
    /// Wait budget in seconds when the timer is enabled (positive integer; default: 1800 / 30 minutes).
    pub timeout_secs: Option<u64>,
}

/// User configurable settings for the web_fetch tool (`[toolset.web_fetch]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebFetchToolConfig {
    /// When set, all HTTP requests are routed through this URL.
    /// Resolution: TOML > `GROK_WEB_FETCH_PROXY` env > remote settings > None.
    pub proxy_endpoint: Option<String>,
    /// When set, it overrides the built-in default allowlist.
    /// An explicit empty list blocks all fetches.
    /// Resolution: TOML > remote settings > built-in defaults.
    pub allowed_domains: Option<Vec<String>>,
    /// Allow fetches to explicit loopback hosts only (`localhost` / `127.0.0.0/8` / `::1`).
    /// Private and metadata ranges stay blocked.
    /// Resolution: TOML > `GROK_WEB_FETCH_ALLOW_LOCAL` env > false.
    pub allow_local: Option<bool>,
}

impl WebFetchToolConfig {
    /// Resolve `WebFetchParams` by merging TOML > env > remote settings layers.
    /// `remote_proxy` and `remote_domains` are the remote settings fallback values from `RemoteSettings`.
    /// `context_window` comes from the session's SamplingConfig (model-provided).
    pub(crate) fn resolve_params(
        &self,
        remote_proxy: Option<&str>,
        remote_domains: Option<&[String]>,
        context_window_tokens: Option<u64>,
    ) -> xai_grok_tools::implementations::grok_build::web_fetch::WebFetchParams {
        use crate::agent::config::env_string;

        let proxy_endpoint = self
            .proxy_endpoint
            .as_ref()
            .cloned()
            .or_else(|| env_string("GROK_WEB_FETCH_PROXY"))
            .or_else(|| remote_proxy.map(|s| s.to_owned()));

        let allowed_domains = self
            .allowed_domains
            .as_ref()
            .cloned()
            .or_else(|| remote_domains.map(|d| d.to_vec()));

        let allow_local = self
            .allow_local
            .or_else(|| xai_grok_config::env_bool("GROK_WEB_FETCH_ALLOW_LOCAL"));

        xai_grok_tools::implementations::grok_build::web_fetch::WebFetchParams {
            proxy_endpoint,
            allowed_domains,
            context_window_tokens,
            allow_local,
            ..Default::default()
        }
    }
}

/// This is the *shell-side* config that holds sampling-level settings (e.g., web search API key from the sampling client).
/// It is distinct from `xai_grok_tools::registry::types::ToolsetConfig` which holds tool-implementation-level config (bash limits, web search mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellToolsetConfig {
    pub bash: BashToolConfig,
    pub web_search: SamplerConfig,
    #[serde(default)]
    pub web_fetch: WebFetchToolConfig,
    #[serde(default)]
    pub ask_user_question: AskUserQuestionToolConfig,
    /// Which file-operation toolset to use: `"standard"` (default) or `"hashline"`.
    #[serde(default)]
    pub file_toolset: FileToolset,
    /// Only used when `file_toolset = "hashline"`.
    #[serde(default)]
    pub hashline: HashlineSchemeConfig,
}

impl Default for ShellToolsetConfig {
    fn default() -> Self {
        Self::new(None, None)
    }
}

pub(crate) fn web_search_sampling_config(base: SamplerConfig) -> SamplerConfig {
    let model = if base.model.is_empty() {
        models::default_web_search_model().to_string()
    } else {
        base.model.clone()
    };
    SamplerConfig {
        model,
        max_completion_tokens: Some(8192),
        temperature: Some(0.1),
        top_p: Some(0.95),
        force_http1: false,
        max_retries: None,
        ..base
    }
}

impl ShellToolsetConfig {
    /// Optionally layers sampling credentials onto the web search config.
    pub fn new(base: Option<Self>, sampling_config: Option<SamplerConfig>) -> Self {
        let default_base = SamplerConfig {
            api_key: None,
            base_url: "https://api.x.ai/v1".to_string(),
            mtls_cert_dir: None,
            model: String::new(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            auth_scheme: Default::default(),
            request_compression: Default::default(),
            extra_headers: indexmap::IndexMap::new(),
            extra_response_includes: Vec::new(),
            query_params: indexmap::IndexMap::new(),
            env_http_headers: indexmap::IndexMap::new(),
            context_window: 256_000,
            client_version: None,
            reasoning_effort: None,
            reasoning_shape: Default::default(),
            reasoning_summary: None,
            force_http1: false,
            max_retries: None,
            rate_limit_retry_threshold: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            conversation_group_id: None,
            origin_client: None,
            // Leaving the callback `None` here is fine; this base is only the placeholder for the "no base provided" path
            // Production `SamplerConfig`s in agent/config.rs and acp_session.rs set the real attribution callback
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        };
        let mut toolset = base.unwrap_or_else(|| Self {
            bash: BashToolConfig::default(),
            web_search: web_search_sampling_config(default_base),
            web_fetch: WebFetchToolConfig::default(),
            ask_user_question: AskUserQuestionToolConfig::default(),
            file_toolset: FileToolset::default(),
            hashline: HashlineSchemeConfig::default(),
        });
        if let Some(sc) = sampling_config {
            toolset.web_search = web_search_sampling_config(sc);
        }
        toolset
    }

    /// Local config takes precedence; remote `/v1/settings` is used as fallback when local is the default.
    pub(crate) fn resolve_file_toolset(
        &self,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) -> FileToolset {
        if self.file_toolset != FileToolset::Standard {
            return self.file_toolset;
        }
        if let Some(remote) = remote
            && remote.file_toolset.as_deref() == Some("hashline")
        {
            return FileToolset::Hashline;
        }
        self.file_toolset
    }
}

// ---------------------------------------------------------------------------
// File toolset selection
// ---------------------------------------------------------------------------

/// Configurable in `config.toml` under `[toolset.hashline]`:
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HashlineSchemeConfig {
    /// Active scheme: `"chunk"` (default) or `"content_only"`.
    pub scheme: String,
    /// Anchor hash length in characters (1-4).
    pub hash_len: usize,
    /// Chunk size for the chunk scheme.
    pub chunk_size: usize,
}

impl Default for HashlineSchemeConfig {
    fn default() -> Self {
        Self {
            scheme: "chunk".to_owned(),
            hash_len: 3,
            chunk_size: 8,
        }
    }
}

impl HashlineSchemeConfig {
    pub fn validate(&self) -> Result<(), String> {
        match self.scheme.as_str() {
            "chunk" | "content_only" => {}
            other => {
                return Err(format!(
                    "unknown hashline scheme \"{other}\": expected \"chunk\" or \"content_only\""
                ));
            }
        }
        if self.hash_len == 0 || self.hash_len > 4 {
            return Err(format!(
                "hashline hash_len must be 1..=4, got {}",
                self.hash_len
            ));
        }
        if self.scheme == "chunk" && self.chunk_size == 0 {
            return Err("hashline chunk_size must be > 0".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileToolset {
    /// Standard toolset: read_file, search_replace, grep.
    #[default]
    Standard,
    /// Hashline toolset: hashline_read, hashline_edit, hashline_grep.
    Hashline,
}

impl FileToolset {
    /// For hashline, the scheme params are copied into each tool's params.
    /// Returns an error if hashline config is invalid (upfront validation).
    pub fn tool_configs(
        self,
        hashline_config: &HashlineSchemeConfig,
    ) -> Result<Vec<ToolConfig>, String> {
        match self {
            Self::Standard => Ok(vec![
                ToolConfig::for_tool::<grok_build::ReadFileTool>(),
                ToolConfig::for_tool::<grok_build::SearchReplaceTool>(),
                ToolConfig::for_tool::<grok_build::GrepTool>(),
            ]),
            Self::Hashline => {
                hashline_config.validate()?;
                let params_json = serde_json::json!({
                    "scheme": hashline_config.scheme,
                    "hash_len": hashline_config.hash_len,
                    "chunk_size": hashline_config.chunk_size,
                });
                let params_map = match params_json {
                    serde_json::Value::Object(m) => Some(m),
                    _ => None,
                };
                Ok(vec![
                    ToolConfig {
                        id: "GrokBuildHashline:hashline_read".to_owned(),
                        params: params_map.clone(),
                        name_override: None,
                        params_name_overrides: None,
                        description_override: None,
                        behavior_version: None,
                        kind: None,
                    },
                    ToolConfig {
                        id: "GrokBuildHashline:hashline_edit".to_owned(),
                        params: params_map.clone(),
                        name_override: None,
                        params_name_overrides: None,
                        description_override: None,
                        behavior_version: None,
                        kind: None,
                    },
                    ToolConfig {
                        id: "GrokBuildHashline:hashline_grep".to_owned(),
                        params: params_map,
                        name_override: None,
                        params_name_overrides: None,
                        description_override: None,
                        behavior_version: None,
                        kind: None,
                    },
                ])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_toolset_default_is_standard() {
        assert_eq!(FileToolset::default(), FileToolset::Standard);
    }

    #[test]
    fn standard_toolset_configs() {
        let configs = FileToolset::Standard
            .tool_configs(&HashlineSchemeConfig::default())
            .unwrap();
        assert_eq!(configs.len(), 3);
        let ids: Vec<&str> = configs.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"GrokBuild:read_file"));
        assert!(ids.contains(&"GrokBuild:search_replace"));
        assert!(ids.contains(&"GrokBuild:grep"));
    }

    #[test]
    fn hashline_toolset_configs() {
        let configs = FileToolset::Hashline
            .tool_configs(&HashlineSchemeConfig::default())
            .unwrap();
        assert_eq!(configs.len(), 3);
        let ids: Vec<&str> = configs.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"GrokBuildHashline:hashline_read"));
        assert!(ids.contains(&"GrokBuildHashline:hashline_edit"));
        assert!(ids.contains(&"GrokBuildHashline:hashline_grep"));
    }

    /// Plan/explore omit `search_replace` by contract ("no Write/Edit/MultiEdit"); the hashline override must not hand it back as `hashline_edit`.
    #[test]
    fn file_toolset_override_never_grants_edit_to_read_only_toolsets() {
        let file_tools = FileToolset::Hashline
            .tool_configs(&HashlineSchemeConfig::default())
            .unwrap();

        for mut def in [
            xai_grok_agent::config::AgentDefinition::plan(),
            xai_grok_agent::config::AgentDefinition::explore(),
        ] {
            let name = def.name.clone();
            assert!(
                !def.tool_config
                    .tools
                    .iter()
                    .any(|t| t.id == "GrokBuild:search_replace"),
                "{name}: fixture must be read-only before the override"
            );
            def.override_file_tools(file_tools.clone());
            let ids: Vec<&str> = def
                .tool_config
                .tools
                .iter()
                .map(|t| t.id.as_str())
                .collect();
            // The swap engages (read moves to hashline)...
            assert!(
                ids.contains(&"GrokBuildHashline:hashline_read"),
                "{name}: {ids:?}"
            );
            assert!(!ids.contains(&"GrokBuild:read_file"), "{name}: {ids:?}");
            // ...but never grants the edit slot.
            assert!(
                !ids.contains(&"GrokBuildHashline:hashline_edit"),
                "{name}: override granted an edit tool to a no-edit toolset: {ids:?}"
            );
        }
    }

    #[test]
    fn hashline_configs_carry_scheme_params() {
        let cfg = HashlineSchemeConfig {
            scheme: "content_only".to_owned(),
            hash_len: 2,
            chunk_size: 16,
        };
        let configs = FileToolset::Hashline.tool_configs(&cfg).unwrap();
        for tc in &configs {
            let params = tc
                .params
                .as_ref()
                .expect("hashline tools should have params");
            assert_eq!(
                params.get("scheme").and_then(|v| v.as_str()),
                Some("content_only")
            );
            assert_eq!(params.get("hash_len").and_then(|v| v.as_u64()), Some(2));
            assert_eq!(params.get("chunk_size").and_then(|v| v.as_u64()), Some(16));
        }
    }

    #[test]
    fn file_toolset_deserializes_from_string() {
        let standard: FileToolset = serde_json::from_str("\"standard\"").unwrap();
        assert_eq!(standard, FileToolset::Standard);
        let hashline: FileToolset = serde_json::from_str("\"hashline\"").unwrap();
        assert_eq!(hashline, FileToolset::Hashline);
    }

    // -- resolve_file_toolset precedence tests ---

    #[test]
    fn resolve_local_hashline_wins_over_remote() {
        let cfg = ShellToolsetConfig {
            file_toolset: FileToolset::Hashline,
            ..ShellToolsetConfig::default()
        };
        assert_eq!(cfg.resolve_file_toolset(None), FileToolset::Hashline,);
    }

    #[test]
    fn resolve_remote_used_when_local_default() {
        let cfg = ShellToolsetConfig::default(); // Standard (default)
        let remote = crate::util::config::RemoteSettings {
            file_toolset: Some("hashline".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_file_toolset(Some(&remote)),
            FileToolset::Hashline,
        );
    }

    #[test]
    fn resolve_default_when_no_remote() {
        let cfg = ShellToolsetConfig::default();
        assert_eq!(cfg.resolve_file_toolset(None), FileToolset::Standard,);
    }

    #[test]
    fn invalid_hashline_config_rejected_upfront() {
        let bad = HashlineSchemeConfig {
            scheme: "bogus".to_owned(),
            hash_len: 3,
            chunk_size: 8,
        };
        let err = FileToolset::Hashline.tool_configs(&bad);
        assert!(err.is_err(), "unknown scheme should be rejected upfront");
        assert!(err.unwrap_err().contains("unknown"));
    }

    #[test]
    fn invalid_hash_len_rejected_upfront() {
        let bad = HashlineSchemeConfig {
            scheme: "chunk".to_owned(),
            hash_len: 0,
            chunk_size: 8,
        };
        assert!(FileToolset::Hashline.tool_configs(&bad).is_err());
    }

    // -- resolve_params precedence tests ---

    #[test]
    fn resolve_params_toml_wins_over_remote() {
        let local = WebFetchToolConfig {
            proxy_endpoint: Some("https://toml-proxy.example.com".to_owned()),
            allowed_domains: Some(vec!["toml.example.com".to_owned()]),
            allow_local: Some(true),
        };
        let params = local.resolve_params(
            Some("https://remote-proxy.example.com"),
            Some(&["remote.example.com".to_owned()]),
            None,
        );
        assert_eq!(
            params.proxy_endpoint.as_deref(),
            Some("https://toml-proxy.example.com")
        );
        assert_eq!(
            params.allowed_domains,
            Some(vec!["toml.example.com".to_owned()])
        );
        assert_eq!(params.allow_local, Some(true));
        assert!(params.allow_local());
    }

    #[test]
    fn resolve_params_remote_used_when_toml_absent() {
        let local = WebFetchToolConfig::default();
        let params = local.resolve_params(
            Some("https://remote-proxy.example.com"),
            Some(&["remote.example.com".to_owned()]),
            None,
        );
        assert_eq!(
            params.proxy_endpoint.as_deref(),
            Some("https://remote-proxy.example.com")
        );
        assert_eq!(
            params.allowed_domains,
            Some(vec!["remote.example.com".to_owned()])
        );
        assert!(!params.allow_local());
    }

    #[test]
    fn resolve_params_defaults_when_nothing_set() {
        let local = WebFetchToolConfig::default();
        let params = local.resolve_params(None, None, None);
        assert!(params.proxy_endpoint.is_none());
        assert!(params.allowed_domains.is_none());
        assert!(!params.allow_local());
    }

    #[test]
    fn resolve_params_toml_empty_domains_blocks_all() {
        let local = WebFetchToolConfig {
            proxy_endpoint: None,
            allowed_domains: Some(vec![]),
            allow_local: None,
        };
        let params = local.resolve_params(None, Some(&["remote.example.com".to_owned()]), None);
        assert_eq!(params.allowed_domains, Some(vec![]));
    }

    // -- to_bash_params_json allow_background_operator precedence (local > remote > true) --

    fn allow_bg_op(map: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
        map.get("allow_background_operator")
            .and_then(|v| v.as_bool())
    }

    #[test]
    fn allow_background_operator_local_wins_over_remote() {
        let local = BashToolConfig {
            allow_background_operator: Some(false),
            ..BashToolConfig::default()
        };
        assert_eq!(
            allow_bg_op(&local.to_bash_params_json(None, Some(true))),
            Some(false)
        );
    }

    #[test]
    fn allow_background_operator_remote_used_when_local_absent() {
        let local = BashToolConfig::default();
        assert_eq!(
            allow_bg_op(&local.to_bash_params_json(None, Some(false))),
            Some(false)
        );
    }

    #[test]
    fn allow_background_operator_defaults_true_when_unset() {
        let local = BashToolConfig::default();
        assert_eq!(
            allow_bg_op(&local.to_bash_params_json(None, None)),
            Some(true)
        );
    }

    // -- max_timeout_secs: production sets the 10h foreground ceiling explicitly (also the binary default); overridable via config.toml --

    fn max_timeout(map: &serde_json::Map<String, serde_json::Value>) -> Option<f64> {
        map.get("max_timeout_secs").and_then(|v| v.as_f64())
    }

    #[test]
    fn max_timeout_secs_defaults_to_production_10h() {
        let local = BashToolConfig::default();
        assert_eq!(
            max_timeout(&local.to_bash_params_json(None, None)),
            Some(PRODUCTION_MAX_TIMEOUT_SECS),
            "production grok-build must set the 10h foreground ceiling"
        );
    }

    #[test]
    fn max_timeout_secs_local_override_wins() {
        let local = BashToolConfig {
            max_timeout_secs: Some(300.0),
            ..BashToolConfig::default()
        };
        assert_eq!(
            max_timeout(&local.to_bash_params_json(None, None)),
            Some(300.0),
        );
    }

    // -- foreground_block_budget_ms: only emitted when set (server defaults to 15s) --

    fn fg_budget(map: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
        map.get("foreground_block_budget_ms")
            .and_then(|v| v.as_u64())
    }

    #[test]
    fn foreground_block_budget_ms_omitted_by_default() {
        let local = BashToolConfig::default();
        assert!(
            fg_budget(&local.to_bash_params_json(None, None)).is_none(),
            "unset budget must not be sent (server keeps 15s default)"
        );
    }

    #[test]
    fn foreground_block_budget_ms_local_override_emitted() {
        let local = BashToolConfig {
            foreground_block_budget_ms: Some(0),
            ..BashToolConfig::default()
        };
        assert_eq!(fg_budget(&local.to_bash_params_json(None, None)), Some(0),);
        let local = BashToolConfig {
            foreground_block_budget_ms: Some(30_000),
            ..BashToolConfig::default()
        };
        assert_eq!(
            fg_budget(&local.to_bash_params_json(None, None)),
            Some(30_000),
        );
    }
}
