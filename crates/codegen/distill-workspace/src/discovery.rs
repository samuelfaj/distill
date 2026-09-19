// Modified for Distill by Samuel Fajreldines, 2026.
//! Skill, plugin, project-config, and permissions discovery.
//!
//! Delegates to the existing discovery implementations in `distill-agent` and `distill-tools` rather than duplicating them.
//! The workspace stores configuration on [`WorkspaceShared`] and the [`MpscChannel`] methods call these helpers.
//!
//! Re-exports [`AgentsMdTracker`] from `distill-tools` for external consumers that need per-session project-instruction tracking.

use std::path::Path;

use serde_json::Value;
use tracing::Instrument as _;

// Re-export AgentsMdTracker so consumers can reference it via the workspace crate without a direct distill-tools dependency
pub use distill_tools::types::agents_md_tracker::AgentsMdTracker;

// Re-export the config types that callers pass into WorkspaceConfig.
pub use distill_agent::plugins::discovery::DiscoveryConfig as PluginDiscoveryConfig;
pub use distill_agent::plugins::trust::TrustStore as PluginTrustStore;
pub use distill_agent::prompt::skills::SkillsConfig;

/// Discover skills visible from the workspace root via `list_skills`, each [`SkillInfo`] as JSON.
/// That walk holds no async locks across `.await`, so contention is not a concern.
/// `project_trusted` omits project-scope skills when false.
pub async fn discover_skills(
    root_cwd: &Path,
    config: &SkillsConfig,
    project_trusted: bool,
) -> Vec<Value> {
    let span = distill_telemetry::region::Region::from_span(tracing::info_span!(
        "workspace.discover_skills",
        skill_count = tracing::field::Empty,
    ));
    let cwd_str = root_cwd.to_string_lossy();
    // Workspace discovery does no per-vendor compat gating; pass the all-on default
    let skills = distill_agent::prompt::skills::list_skills(
        Some(&cwd_str),
        config,
        distill_agent::prompt::skills::CompatConfig::default(),
        project_trusted,
    )
    .instrument(span.span().clone())
    .await;
    span.span().record("skill_count", skills.len() as i64);

    skills
        .into_iter()
        .filter_map(|s| match serde_json::to_value(&s) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    skill = %s.name,
                    error = %e,
                    "failed to serialize SkillInfo; dropping from response"
                );
                None
            }
        })
        .collect()
}

/// Discover project-instruction files (AGENTS.md, Claude.md, rules) from the workspace root up to the git root.
/// `project_trusted` is the folder-trust verdict for `root_cwd`; when false, project-scope instructions are omitted.
pub async fn discover_agents_md(root_cwd: &Path, project_trusted: bool) -> Vec<Value> {
    let span = distill_telemetry::region::Region::from_span(tracing::info_span!(
        "workspace.discover_agents_md"
    ));
    let cwd_str = root_cwd.to_string_lossy();
    // No user config is loaded on this path, so the vendor and `[paths]` defaults apply
    let files = distill_agent::prompt::agents_md::read_agents_config_with_paths(
        &cwd_str,
        distill_tools::types::compat::CompatConfig::default(),
        &distill_agent::prompt::paths::PathsConfig::default(),
        project_trusted,
    )
    .instrument(span.span().clone())
    .await;

    files
        .into_iter()
        .filter_map(|file| match serde_json::to_value(&file) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    file = %file.file_path,
                    error = %e,
                    "failed to serialize AgentConfigFile; dropping from response"
                );
                None
            }
        })
        .collect()
}

/// Discover plugins visible from the workspace root. [`DiscoveredPlugin`] is not `Serialize`, so each is converted to the JSON fields consumers need.
/// `project_trusted` gates Project-scope plugins.
pub fn discover_plugins(
    root_cwd: &Path,
    config: &PluginDiscoveryConfig,
    trust_store: &PluginTrustStore,
    project_trusted: bool,
) -> Vec<Value> {
    let _span = tracing::info_span!(
        "workspace.discover_plugins",
        plugin_count = tracing::field::Empty
    )
    .entered();
    let discovered = distill_agent::plugins::discover_plugins(
        Some(root_cwd),
        config,
        trust_store,
        project_trusted,
    );
    tracing::Span::current().record("plugin_count", discovered.len() as i64);

    discovered
        .into_iter()
        .map(|dp| {
            serde_json::json!({
                "name": dp.manifest.name,
                "id": dp.id.0,
                "root": dp.root.to_string_lossy(),
                "scope": dp.scope.to_string(),
                "trusted": dp.trusted,
                "version": dp.manifest.version,
                "description": dp.manifest.description,
                "has_skills": !dp.skill_dirs.is_empty(),
                "has_agents": !dp.agent_dirs.is_empty(),
                "has_hooks": dp.hooks_path.is_some(),
                "has_mcp": dp.mcp_config_path.is_some(),
                "has_lsp": dp.lsp_config_path.is_some(),
                "conflict": dp.conflict,
            })
        })
        .collect()
}

/// Load project config from `<root_cwd>/.grok/config.toml`.
/// Missing or unparseable files return `Value::Null`; non-fatal errors are logged.
pub fn load_project_config(root_cwd: &Path) -> Value {
    let config_path = root_cwd.join(".grok").join("config.toml");
    match distill_config::load_config_file(&config_path) {
        Ok(toml::Value::Table(ref t)) if t.is_empty() => {
            // The config loader returns an empty table when the file does not exist
            // Normalize to Null for callers
            Value::Null
        }
        Ok(toml_val) => toml_to_json(&toml_val),
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "failed to load project config"
            );
            Value::Null
        }
    }
}

/// TOML maps onto JSON directly except `Datetime`, which becomes a JSON string.
fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            let map: serde_json::Map<String, Value> = table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect();
            Value::Object(map)
        }
    }
}

/// Effective permission configuration, merging the usual settings layers via [`resolution::resolve_permissions_with_provenance`].
/// `project_trusted` gates project-tier sources (same contract as env/hooks/plugins); hub/cloud callers outside that model should pass `true`.
/// Returns `sources`/`loaded`/`skipped`, or `Value::Null` if nothing is configured.
pub async fn load_permissions(root_cwd: &Path, project_trusted: bool) -> Value {
    use crate::permission::resolution;

    let Some(resolved) = resolution::resolve_permissions_with_provenance(root_cwd, project_trusted)
        .await
        .resolved
    else {
        return Value::Null;
    };

    let mut sources: Vec<String> = Vec::new();
    for s in resolved.sources.iter().map(|s| s.to_string()) {
        if !sources.contains(&s) {
            sources.push(s);
        }
    }

    let skipped: Vec<Value> = resolved
        .skipped
        .iter()
        .map(|s| {
            serde_json::json!({
                "rule": s.rule,
                "reason": s.reason,
            })
        })
        .collect();

    serde_json::json!({
        "sources": sources,
        "loaded": resolved.config.rules.len(),
        "skipped": skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Note: `list_skills` also discovers user-scoped skills from
    // `~/.grok/skills/`, so on a developer machine the result may be
    // non-empty even for an empty workspace. Tests below check for specific skills rather than asserting emptiness.

    #[tokio::test]
    async fn discover_skills_finds_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".grok").join("skills").join("my-skill");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill\n---\n# Body\n",
        )
        .unwrap();

        let skills = discover_skills(
            tmp.path(),
            &SkillsConfig::default(),
            /*project_trusted*/ true,
        )
        .await;
        let found = skills
            .iter()
            .find(|s| s.get("name").and_then(|v| v.as_str()) == Some("my-skill"));
        assert!(found.is_some(), "should find my-skill");
        let skill = found.unwrap();
        assert_eq!(
            skill.get("description").and_then(|v| v.as_str()),
            Some("A test skill")
        );
    }

    #[tokio::test]
    async fn discover_skills_respects_ignore_config() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".grok").join("skills").join("ignored");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: ignored\ndescription: Ignored\n---\n",
        )
        .unwrap();

        let config = SkillsConfig {
            paths: vec![],
            ignore: vec![tmp.path().to_string_lossy().to_string()],
            disabled: vec![],
            server_skill_dirs: vec![],
            bundled_skill_dirs: vec![],
        };
        let skills = discover_skills(tmp.path(), &config, /*project_trusted*/ true).await;
        let found = skills
            .iter()
            .any(|s| s.get("name").and_then(|v| v.as_str()) == Some("ignored"));
        assert!(
            !found,
            "skill in ignore path should be filtered out of results"
        );
    }

    #[tokio::test]
    async fn discover_skills_returns_serialized_values() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp
            .path()
            .join(".grok")
            .join("skills")
            .join("serialized-check");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: serialized-check\ndescription: Test serialization\n---\n",
        )
        .unwrap();

        let skills = discover_skills(
            tmp.path(),
            &SkillsConfig::default(),
            /*project_trusted*/ true,
        )
        .await;
        let found = skills
            .iter()
            .find(|s| s.get("name").and_then(|v| v.as_str()) == Some("serialized-check"))
            .expect("should find serialized-check");
        // SkillInfo has these required fields when serialized
        assert!(
            found.get("path").is_some_and(|v| v.is_string()),
            "path should be a string"
        );
        assert!(
            found.get("scope").is_some_and(|v| v.is_string()),
            "scope should be serialized"
        );
    }

    #[test]
    fn agent_config_file_wire_matches_workspace_types_mirror() {
        // The RPC serializes distill's AgentConfigFile and the remote consumer deserializes the workspace-types mirror
        // Pin the cross-crate serde shape so a rename/attr drift on either side can't silently break discovery
        let src = distill_agent::prompt::agents_md::AgentConfigFile {
            file_name: "AGENTS.md".to_string(),
            file_path: "/repo/AGENTS.md".to_string(),
            content: "# Instructions\n".to_string(),
            source: distill_agent::prompt::agents_md::InstructionSource::Configured,
        };
        let json = serde_json::to_value(&src).unwrap();
        let mirror: distill_workspace_types::rpc::agents_md::AgentConfigFile =
            serde_json::from_value(json.clone())
                .expect("server shape must deserialize into mirror");
        assert_eq!(mirror.file_name, src.file_name);
        assert_eq!(mirror.file_path, src.file_path);
        assert_eq!(mirror.content, src.content);
        assert_eq!(
            mirror.source,
            distill_workspace_types::rpc::agents_md::InstructionSource::Configured
        );
        assert_eq!(
            serde_json::to_value(&mirror).unwrap(),
            json,
            "mirror must serialize to the exact shape the server emits"
        );
    }

    // Discovery also scans the real `~/.grok`, so fixtures use test-unique names.
    #[tokio::test]
    async fn discover_agents_md_receives_normalized_rule_content() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join(".cursor").join("rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(
            rules_dir.join("xyzzy-discover-agents-md-test.md"),
            "---\nglobs: [\"*.rs\"]\n---\nUse tabs, not spaces.\n",
        )
        .unwrap();

        let files = discover_agents_md(tmp.path(), /*project_trusted*/ true).await;
        let rule = files
            .iter()
            .find(|f| {
                f.get("file_path")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| p.ends_with("/.cursor/rules/xyzzy-discover-agents-md-test.md"))
            })
            .expect("should discover the rules file");
        let Some(content) = rule.get("content").and_then(|v| v.as_str()) else {
            panic!("rule content missing: {rule}");
        };
        assert!(
            content.contains("Use tabs, not spaces."),
            "rule body must survive: {content}"
        );
        assert!(
            !content.contains("globs:"),
            "YAML frontmatter must be stripped: {content}"
        );
    }

    #[tokio::test]
    async fn discover_agents_md_keeps_non_rules_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("AGENTS.md"),
            "---\nXYZZY_LEADING_DASHES_MARKER\n---\nBody after horizontal rules.\n",
        )
        .unwrap();

        let files = discover_agents_md(tmp.path(), /*project_trusted*/ true).await;
        let agents = files
            .iter()
            .find(|f| {
                f.get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|c| c.contains("XYZZY_LEADING_DASHES_MARKER"))
            })
            .expect("should discover the AGENTS.md with its leading dashes intact");
        let Some(content) = agents.get("content").and_then(|v| v.as_str()) else {
            panic!("agents content missing: {agents}");
        };
        assert!(
            content.starts_with("---\n"),
            "stripping must be rules-files-only: {content}"
        );
    }

    // Note: `discover_plugins` also discovers user-scoped plugins
    // from `~/.grok/plugins/`, so tests check for specific plugins.

    #[test]
    fn discover_plugins_finds_manifest_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".grok").join("plugins").join("test-plugin");
        fs::create_dir_all(&plugins_dir).unwrap();
        fs::write(
            plugins_dir.join("plugin.json"),
            r#"{"name": "test-plugin"}"#,
        )
        .unwrap();
        // Plugin needs at least one component to be recognized
        fs::create_dir_all(plugins_dir.join("skills")).unwrap();

        let trust = PluginTrustStore::load_from(tmp.path().join("trust"));
        let config = PluginDiscoveryConfig::default();
        let plugins = discover_plugins(tmp.path(), &config, &trust, true);
        let found = plugins
            .iter()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-plugin"));
        assert!(found.is_some(), "should find test-plugin");
        let p = found.unwrap();
        assert_eq!(p.get("scope").and_then(|v| v.as_str()), Some("project"));
        assert_eq!(p.get("has_skills").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn discover_plugins_json_has_expected_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".grok").join("plugins").join("field-test");
        fs::create_dir_all(plugins_dir.join("skills")).unwrap();
        fs::write(
            plugins_dir.join("plugin.json"),
            r#"{"name": "field-test", "version": "1.0.0", "description": "Test plugin"}"#,
        )
        .unwrap();

        let trust = PluginTrustStore::load_from(tmp.path().join("trust"));
        let config = PluginDiscoveryConfig::default();
        let plugins = discover_plugins(tmp.path(), &config, &trust, true);
        let p = plugins
            .iter()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("field-test"))
            .expect("should find field-test");
        assert_eq!(p.get("version").and_then(|v| v.as_str()), Some("1.0.0"));
        assert_eq!(
            p.get("description").and_then(|v| v.as_str()),
            Some("Test plugin")
        );
        assert!(p.get("id").is_some_and(|v| v.is_string()));
        assert!(p.get("root").is_some_and(|v| v.is_string()));
        assert!(p.get("trusted").is_some_and(|v| v.is_boolean()));
        assert!(p.get("has_hooks").is_some_and(|v| v.is_boolean()));
        assert!(p.get("has_mcp").is_some_and(|v| v.is_boolean()));
        assert!(p.get("has_lsp").is_some_and(|v| v.is_boolean()));
        assert!(p.get("has_agents").is_some_and(|v| v.is_boolean()));
        assert!(p.get("has_skills").is_some_and(|v| v.is_boolean()));
    }

    #[test]
    fn load_project_config_missing_file_returns_null() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load_project_config(tmp.path());
        assert_eq!(config, Value::Null, "missing config → Null");
    }

    #[test]
    fn load_project_config_reads_toml_as_json() {
        let tmp = tempfile::tempdir().unwrap();
        let grok_dir = tmp.path().join(".grok");
        fs::create_dir_all(&grok_dir).unwrap();
        fs::write(
            grok_dir.join("config.toml"),
            "[skills]\npaths = [\"/extra/skills\"]\n\n[plugins]\ndisabled = [\"noisy-plugin\"]\n",
        )
        .unwrap();

        let config = load_project_config(tmp.path());
        assert!(config.is_object(), "parsed config should be an object");
        assert!(
            config
                .pointer("/skills/paths")
                .is_some_and(|v| v.is_array()),
            "skills.paths should be an array"
        );
        assert_eq!(
            config.pointer("/skills/paths/0").and_then(|v| v.as_str()),
            Some("/extra/skills")
        );
        assert_eq!(
            config
                .pointer("/plugins/disabled/0")
                .and_then(|v| v.as_str()),
            Some("noisy-plugin")
        );
    }

    #[test]
    fn toml_to_json_basic_types() {
        let toml_val: toml::Value =
            toml::from_str("s = \"hello\"\ni = 42\nf = 3.14\nb = true\n").unwrap();
        let json = toml_to_json(&toml_val);
        assert_eq!(json.get("s").and_then(|v| v.as_str()), Some("hello"));
        assert_eq!(json.get("i").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(json.get("b").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn toml_to_json_nested_table() {
        let toml_val: toml::Value = toml::from_str("[section]\nkey = \"value\"\n").unwrap();
        let json = toml_to_json(&toml_val);
        assert_eq!(
            json.pointer("/section/key").and_then(|v| v.as_str()),
            Some("value")
        );
    }

    #[test]
    fn toml_to_json_array() {
        let toml_val: toml::Value = toml::from_str("items = [1, 2, 3]\n").unwrap();
        let json = toml_to_json(&toml_val);
        assert_eq!(json.pointer("/items/0").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(json.pointer("/items/2").and_then(|v| v.as_i64()), Some(3));
    }

    #[tokio::test]
    async fn load_permissions_with_settings_file_returns_object() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a minimal .claude/settings.json with a permission rule so the test always exercises the non-null path
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        )
        .unwrap();

        let result = load_permissions(tmp.path(), true).await;
        assert!(result.is_object(), "should return an object, got {result}");
        assert!(
            result.get("sources").is_some_and(|v| v.is_array()),
            "sources should be an array"
        );
        assert!(
            result.get("loaded").is_some_and(|v| v.is_number()),
            "loaded should be a number"
        );
        assert!(
            result.get("skipped").is_some_and(|v| v.is_array()),
            "skipped should be an array"
        );
        assert!(
            result.get("loaded").and_then(|v| v.as_u64()).unwrap_or(0) >= 1,
            "should have at least one loaded rule"
        );
    }
}
