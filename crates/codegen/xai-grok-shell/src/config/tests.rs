#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
fn with_env_var<T>(name: &str, value: &str, f: impl FnOnce() -> T) -> T {
    let previous = std::env::var(name).ok();
    unsafe {
        std::env::set_var(name, value);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match previous {
        Some(prev) => {
            unsafe {
                std::env::set_var(name, prev);
            }
        }
        None => {
            unsafe {
                std::env::remove_var(name);
            }
        }
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
#[test]
fn expands_env_vars_in_toml_strings() {
    with_env_var(
        "GROK_TEST_CONFIG_EXPAND",
        "expanded",
        || {
            let toml_str = r#"
[mcp_servers.test]
command = "$GROK_TEST_CONFIG_EXPAND/bin/server"
args = ["--path", "${GROK_TEST_CONFIG_EXPAND}/data"]
"#;
            let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
            expand_env_vars_in_toml(&mut value);
            let toml::Value::Table(table) = value else {
                panic!("Expected table root");
            };
            let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
                panic!("Expected mcp_servers table");
            };
            let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
                panic!("Expected test table");
            };
            let command = test.get("command").and_then(|v| v.as_str()).unwrap();
            assert_eq!(command, "expanded/bin/server");
            let args = test.get("args").and_then(|v| v.as_array()).unwrap();
            let Some(arg) = args.get(1) else {
                panic!("expected args[1]: {args:?}");
            };
            assert_eq!(arg.as_str().unwrap(), "expanded/data");
        },
    );
}
#[test]
fn leaves_missing_env_vars_unchanged() {
    let toml_str = r#"
[mcp_servers.test]
command = "$GROK_TEST_CONFIG_MISSING/bin/server"
"#;
    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
    expand_env_vars_in_toml(&mut value);
    let toml::Value::Table(table) = value else {
        panic!("Expected table root");
    };
    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
        panic!("Expected mcp_servers table");
    };
    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
        panic!("Expected test table");
    };
    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
    assert_eq!(command, "$GROK_TEST_CONFIG_MISSING/bin/server");
}
#[test]
fn preserves_literal_dollar_signs() {
    let toml_str = r#"
[mcp_servers.test]
command = "$$HOME"
"#;
    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
    expand_env_vars_in_toml(&mut value);
    let toml::Value::Table(table) = value else {
        panic!("Expected table root");
    };
    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
        panic!("Expected mcp_servers table");
    };
    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
        panic!("Expected test table");
    };
    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
    assert_eq!(command, "$HOME");
}
/// Mutex to serialize tests that touch the GROK_MEMORY env var.
/// Env vars are process-global, so parallel tests race on them.
static MEMORY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Run `f` with `name` set to `value` (Some) or removed (None).
/// Saves and restores the previous value, even on panic.
fn with_env_var_opt<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let previous = std::env::var(name).ok();
    match value {
        Some(v) => unsafe { std::env::set_var(name, v) }
        None => unsafe { std::env::remove_var(name) }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match previous {
        Some(prev) => unsafe { std::env::set_var(name, prev) }
        None => unsafe { std::env::remove_var(name) }
    }
    result.unwrap_or_else(|p| std::panic::resume_unwind(p))
}
/// Run `f` with GROK_MEMORY explicitly unset.
fn without_grok_memory<T>(f: impl FnOnce() -> T) -> T {
    let _guard = MEMORY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("GROK_MEMORY", None, f)
}
/// Run `f` with GROK_MEMORY set to a specific value.
fn with_grok_memory<T>(value: &str, f: impl FnOnce() -> T) -> T {
    let _guard = MEMORY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("GROK_MEMORY", Some(value), f)
}
#[test]
fn memory_config_default_disabled() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(!mem.enabled);
    });
}
#[test]
fn memory_config_legacy_wrapper_matches_tri_state_override() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let enabled = MemoryConfig::resolve(true, false, &config, None);
        let disabled = MemoryConfig::resolve(true, true, &config, None);
        let deferred = MemoryConfig::resolve(false, false, &config, None);
        assert_eq!(
                enabled,
                MemoryConfig::resolve_with_override(Some(true), &config, None)
            );
        assert_eq!(
                disabled,
                MemoryConfig::resolve_with_override(Some(false), &config, None)
            );
        assert_eq!(
                deferred,
                MemoryConfig::resolve_with_override(None, &config, None)
            );
    });
}
#[test]
fn memory_config_from_toml() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str(
                "[memory]\nenabled = false\n[memory_v2]\nenabled = true",
            )
            .unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
        assert_eq!(mem.mode, crate::config::MemoryMode::V2);
    });
}
#[test]
fn standalone_memory_mode_honors_remote_v2_enrollment() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_v2: Some(crate::config::MemoryV2Settings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
                super::resolve_standalone_memory_mode(&config, Some(&remote)),
                crate::config::MemoryMode::V2
            );
    });
}
#[test]
fn public_memory_config_deserializes_with_skipped_defaults() {
    let config: crate::config::MemoryConfig = toml::from_str(
            "enabled = true\n[search]\nmax_results = 9\n[flush]\nenabled = false",
        )
        .unwrap();
    assert!(config.enabled);
    assert_eq!(config.mode, crate::config::MemoryMode::Legacy);
    assert_eq!(config.search.max_results, 9);
    assert_eq!(config.flush, MemoryFlushConfig::default());
    assert_eq!(config.pruning, PruningConfig::default());
    assert_eq!(config.root_dir_override, None);
    assert!(!config.flat_memory_root);
}
#[test]
fn invalid_memory_v2_rollout_is_rejected() {
    let invalid: toml::Value = toml::from_str(
            "[memory_v2]\nenabled = true\nrollout = \"unknown\"",
        )
        .unwrap();
    assert!(crate::agent::config::Config::new_from_toml_cfg(&invalid).is_err());
}
#[test]
fn memory_config_deserializes_through_config() {
    let raw: toml::Value = toml::from_str(
            "[memory]\nenabled = true\n[memory.search]\nmax_results = 9\nunknown_future = 'ignored'",
        )
        .unwrap();
    let config = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    assert_eq!(config.memory.enabled, Some(true));
    assert_eq!(
            config
                .memory
                .search
                .as_ref()
                .and_then(|search| search.max_results),
            Some(9)
        );
    let invalid: toml::Value = toml::from_str("[memory.search]\nmax_results = 'many'")
        .unwrap();
    assert!(crate::agent::config::Config::new_from_toml_cfg(&invalid).is_err());
}
#[test]
fn memory_config_toml_sentinels_beat_remote_values() {
    without_grok_memory(|| {
        let raw: toml::Value = toml::from_str(
                "[memory.embedding]\nmodel = ''\n[memory.dream]\ncheck_interval_secs = 0\n[compaction.memory_flush]\nidle_timeout_secs = 0",
            )
            .unwrap();
        let config = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_embedding_model: Some("remote-model".to_owned()),
            dream_check_interval_secs: Some(900),
            flush_idle_timeout_secs: Some(120),
            ..Default::default()
        };
        let memory = config.resolve_memory(None, Some(&remote));
        assert_eq!(memory.embedding.model, None);
        assert_eq!(memory.dream.check_interval_secs, None);
        assert_eq!(memory.flush.idle_timeout_secs, None);
    });
}
#[test]
fn memory_config_toml_disabled() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(!mem.enabled);
    });
}
#[test]
fn memory_config_env_var_enables() {
    with_grok_memory(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(mem.enabled);
        },
    );
}
#[test]
fn memory_config_env_var_true_enables() {
    with_grok_memory(
        "true",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(mem.enabled);
        },
    );
}
#[test]
fn memory_config_env_var_zero_does_not_enable() {
    with_grok_memory(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(!mem.enabled, "GROK_MEMORY=0 should not enable memory");
        },
    );
}
#[test]
fn memory_config_env_var_false_does_not_enable() {
    with_grok_memory(
        "false",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(!mem.enabled, "GROK_MEMORY=false should not enable memory");
        },
    );
}
#[test]
fn memory_config_cli_overrides_toml_disabled() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let mem = MemoryConfig::resolve(true, false, &config, None);
        assert!(mem.enabled, "CLI flag should override config file");
    });
}
#[test]
fn memory_config_env_zero_force_disables_toml_enabled() {
    with_grok_memory(
        "0",
        || {
            let config: toml::Value = toml::from_str(
                    "[memory]\nenabled = true\n[memory_v2]\nenabled = true",
                )
                .unwrap();
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(
                !mem.enabled,
                "GROK_MEMORY=0 should force-disable both legacy and v2 TOML gates"
            );
        },
    );
}
#[test]
fn memory_config_env_false_force_disables_toml_enabled() {
    with_grok_memory(
        "false",
        || {
            let config: toml::Value = toml::from_str("[memory]\nenabled = true")
                .unwrap();
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(
                !mem.enabled,
                "GROK_MEMORY=false should force-disable even when TOML enables memory"
            );
        },
    );
}
#[test]
fn memory_config_cli_flag_overrides_env_disable() {
    with_grok_memory(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(true, false, &config, None);
            assert!(
                mem.enabled,
                "CLI --experimental-memory should override GROK_MEMORY=0"
            );
        },
    );
}
#[test]
fn memory_config_no_memory_alone_disables() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, true, &config, None);
        assert!(!mem.enabled, "--no-memory alone should disable");
    });
}
#[test]
fn memory_config_no_memory_overrides_env_enable() {
    with_grok_memory(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, true, &config, None);
            assert!(!mem.enabled, "--no-memory should override GROK_MEMORY=1");
        },
    );
}
#[test]
fn memory_config_no_memory_overrides_toml_enabled() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = true").unwrap();
        let mem = MemoryConfig::resolve(false, true, &config, None);
        assert!(!mem.enabled, "--no-memory should override TOML enabled=true");
    });
}
#[test]
fn memory_config_no_memory_overrides_remote_enabled() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            memory_v2: Some(crate::config::MemoryV2Settings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, true, &config, Some(&remote));
        assert!(
                !mem.enabled,
                "--no-memory should override both remote memory gates"
            );
    });
}
#[test]
fn memory_config_defaults_are_correct() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert_eq!(mem.index.max_chunk_chars, 1600);
        assert_eq!(mem.index.chunk_overlap_chars, 320);
        assert_eq!(mem.embedding.provider, "api");
        assert_eq!(mem.embedding.model, None);
        assert_eq!(mem.embedding.dimensions, 1024);
        assert_eq!(mem.search.max_results, 6);
        assert!((mem.search.min_score - 0.7).abs() < f32::EPSILON);
        assert!((mem.search.vector_weight - 0.7).abs() < f32::EPSILON);
        assert!((mem.search.text_weight - 0.3).abs() < f32::EPSILON);
        assert!((mem.search.recency_decay - 0.95).abs() < f32::EPSILON);
        assert!(mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 30.0).abs() < f64::EPSILON);
        assert!(mem.search.mmr.enabled);
        assert!((mem.search.mmr.lambda - 0.7).abs() < f64::EPSILON);
        let weight = |key| {
            mem
                .search
                .source_weights
                .get(key)
                .copied()
                .unwrap_or_else(|| panic!("missing source weight {key}"))
        };
        assert!((weight("workspace") - 1.0).abs() < f32::EPSILON);
        assert!((weight("session") - 1.0).abs() < f32::EPSILON);
        assert!((weight("global") - 1.0).abs() < f32::EPSILON);
        assert!(mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.9));
        assert!(mem.session.save_on_end);
        assert!(mem.flush.enabled);
        assert_eq!(mem.flush.soft_threshold_tokens, 4000);
        assert!(mem.flush.flush_model.is_none());
        assert_eq!(mem.flush.max_flush_write_chars, 8000);
        assert_eq!(mem.flush.idle_timeout_secs, Some(300));
        assert!(mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 3);
        assert_eq!(mem.pruning.soft_trim_threshold, 4000);
        assert_eq!(mem.pruning.soft_trim_head, 1500);
        assert_eq!(mem.pruning.soft_trim_tail, 1500);
        assert_eq!(mem.pruning.hard_clear_age_turns, 10);
        assert!(mem.watcher.enabled);
        assert_eq!(mem.watcher.stale_claim_secs, 60);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 24);
        assert_eq!(mem.dream.min_sessions, 5);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, Some(3600));
    });
}
/// `debounce_ms` was a dead field on `MemoryWatcherConfig` that was never read by any watcher or search path.
/// Verify that existing TOML config files that contain `debounce_ms` still parse without error (serde ignores unknown fields by default).
#[test]
fn memory_config_watcher_debounce_ms_in_toml_is_silently_ignored() {
    without_grok_memory(|| {
        let toml_str = "[memory.watcher]\nenabled = true\ndebounce_ms = 2000\n";
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.watcher.enabled);
        assert_eq!(mem.watcher.stale_claim_secs, 60);
    });
}
#[test]
fn memory_config_full_toml_parsing() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.index]
max_chunk_chars = 2000
chunk_overlap_chars = 400

[memory.embedding]
provider = "local"
model = "all-MiniLM-L6-v2"
dimensions = 384

[memory.search]
max_results = 10
min_score = 0.5
vector_weight = 0.6
text_weight = 0.4
recency_decay = 0.9

[memory.initial_injection]
enabled = false
min_score = 0.8

[memory.search.temporal_decay]
enabled = true
half_life_days = 14.0

[memory.search.source_weights]
workspace = 1.0
session = 0.8
global = 0.5

[memory.session]
save_on_end = false

[compaction.memory_flush]
enabled = false
soft_threshold_tokens = 8000
flush_model = "grok-4"
max_flush_write_chars = 16000
idle_timeout_secs = 300
semantic_dedup_threshold = 0.85

[compaction.pruning]
enabled = false
keep_last_n_turns = 5
soft_trim_threshold = 8000
soft_trim_head = 3000
soft_trim_tail = 3000
hard_clear_age_turns = 20
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
        assert_eq!(mem.index.max_chunk_chars, 2000);
        assert_eq!(mem.index.chunk_overlap_chars, 400);
        assert_eq!(mem.embedding.provider, "local");
        assert_eq!(
                mem.embedding.model.as_deref(),
                Some("all-MiniLM-L6-v2")
            );
        assert_eq!(mem.embedding.dimensions, 384);
        assert_eq!(mem.search.max_results, 10);
        assert!((mem.search.min_score - 0.5).abs() < f32::EPSILON);
        assert!(!mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.8));
        assert!(mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON);
        let Some(&global) = mem.search.source_weights.get("global") else {
            panic!("missing source weight global");
        };
        assert!((global - 0.5).abs() < f32::EPSILON);
        assert!(!mem.session.save_on_end);
        assert!(!mem.flush.enabled);
        assert_eq!(mem.flush.soft_threshold_tokens, 8000);
        assert_eq!(mem.flush.flush_model.as_deref(), Some("grok-4"));
        assert_eq!(mem.flush.max_flush_write_chars, 16000);
        assert_eq!(mem.flush.idle_timeout_secs, Some(300));
        assert_eq!(mem.flush.semantic_dedup_threshold, Some(0.85));
        assert!(!mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 5);
        assert_eq!(mem.pruning.hard_clear_age_turns, 20);
    });
}
#[test]
fn memory_config_partial_toml_uses_defaults_for_missing() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.index]
max_chunk_chars = 3200
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
        assert_eq!(mem.index.max_chunk_chars, 3200);
        assert_eq!(mem.index.chunk_overlap_chars, 320);
        assert_eq!(mem.embedding.dimensions, 1024);
        assert_eq!(mem.search.max_results, 6);
        assert!(mem.flush.enabled);
        assert!(mem.pruning.enabled);
    });
}
#[test]
fn memory_config_remote_settings_enable() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
                mem.enabled,
                "remote memory_enabled=true should enable memory"
            );
    });
}
#[test]
fn memory_config_remote_settings_pruning() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            pruning_enabled: Some(true),
            pruning_keep_last_n_turns: Some(5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 5);
    });
}
#[test]
fn partial_embedding_injection_and_watcher_use_remote_for_missing_fields() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str(
                "[memory.embedding]\ndimensions = 384\n[memory.initial_injection]\nenabled = false\n[memory.watcher]\nstale_claim_secs = 75",
            )
            .unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_embedding_model: Some("remote-model".to_owned()),
            memory_embedding_dimensions: Some(768),
            memory_initial_injection_enabled: Some(true),
            memory_initial_injection_min_score: Some(0.77),
            memory_watcher_enabled: Some(false),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(mem.embedding.model.as_deref(), Some("remote-model"));
        assert_eq!(mem.embedding.dimensions, 384);
        assert!(!mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.77));
        assert!(!mem.watcher.enabled);
        assert_eq!(mem.watcher.stale_claim_secs, 75);
    });
}
#[test]
fn memory_config_local_initial_injection_overrides_remote() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory.initial_injection]
enabled = true
min_score = 0.25
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_initial_injection_enabled: Some(false),
            memory_initial_injection_min_score: Some(0.77),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.25));
    });
}
#[test]
fn memory_config_local_disabled_blocks_remote_enable() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
                !mem.enabled,
                "local [memory] enabled=false should block remote enable"
            );
    });
}
#[test]
fn memory_config_partial_search_uses_remote_for_missing_field() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str("[memory.search]\nmax_results = 20")
            .unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_search_max_results: Some(5),
            memory_search_min_score: Some(0.82),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(mem.search.max_results, 20);
        assert!((mem.search.min_score - 0.82).abs() < f32::EPSILON);
    });
}
#[test]
fn memory_config_remote_none_is_noop() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem_without = MemoryConfig::resolve(false, false, &config, None);
        let mem_with_empty = MemoryConfig::resolve(
            false,
            false,
            &config,
            Some(&crate::util::config::RemoteSettings::default()),
        );
        assert_eq!(
                mem_without.search.max_results,
                mem_with_empty.search.max_results
            );
        assert_eq!(mem_without.enabled, mem_with_empty.enabled);
    });
}
#[test]
fn flush_semantic_dedup_threshold_from_remote_when_no_local_flush() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(0.85),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(
                mem.flush.semantic_dedup_threshold,
                Some(0.85),
                "remote threshold should apply when no local flush config"
            );
    });
}
#[test]
fn flush_semantic_dedup_threshold_clamped_from_remote() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(1.5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(
                mem.flush.semantic_dedup_threshold,
                Some(1.0),
                "remote threshold above 1.0 should be clamped"
            );
        let remote_neg = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(-0.5),
            ..Default::default()
        };
        let mem_neg = MemoryConfig::resolve(false, false, &config, Some(&remote_neg));
        assert_eq!(
                mem_neg.flush.semantic_dedup_threshold,
                Some(0.0),
                "remote threshold below 0.0 should be clamped"
            );
    });
}
#[test]
fn partial_flush_and_pruning_use_remote_for_missing_fields() {
    without_grok_memory(|| {
        let config: toml::Value = toml::from_str(
                "[compaction.memory_flush]\nsemantic_dedup_threshold = 0.88\n[compaction.pruning]\nkeep_last_n_turns = 7",
            )
            .unwrap();
        let remote = crate::util::config::RemoteSettings {
            flush_enabled: Some(false),
            flush_semantic_dedup_threshold: Some(0.70),
            pruning_enabled: Some(false),
            pruning_keep_last_n_turns: Some(2),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(!mem.flush.enabled);
        assert_eq!(mem.flush.semantic_dedup_threshold, Some(0.88));
        assert!(!mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 7);
    });
}
#[test]
fn flush_semantic_dedup_threshold_defaults_to_none() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert_eq!(
                mem.flush.semantic_dedup_threshold, None,
                "threshold should default to None (fallback to compiled-in constant)"
            );
    });
}
#[test]
fn memory_dream_config_defaults() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 24);
        assert_eq!(mem.dream.min_sessions, 5);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, Some(3600));
    });
}
#[test]
fn memory_dream_config_toml_parsing() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory.dream]
enabled = true
min_hours = 12
min_sessions = 3
stale_lock_secs = 1800
check_interval_secs = 600
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 12);
        assert_eq!(mem.dream.min_sessions, 3);
        assert_eq!(mem.dream.stale_lock_secs, 1800);
        assert_eq!(mem.dream.check_interval_secs, Some(600));
    });
}
#[test]
fn memory_dream_config_remote_override_when_toml_absent() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            dream_enabled: Some(true),
            dream_min_hours: Some(48),
            dream_min_sessions: Some(10),
            dream_check_interval_secs: Some(900),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 48);
        assert_eq!(mem.dream.min_sessions, 10);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, Some(900));
    });
}
#[test]
fn memory_dream_config_partial_toml_uses_remote_for_missing_fields() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory.dream]
enabled = false
min_hours = 6
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            dream_enabled: Some(true),
            dream_min_hours: Some(48),
            dream_min_sessions: Some(10),
            dream_check_interval_secs: Some(300),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(!mem.dream.enabled, "local TOML should win over remote");
        assert_eq!(mem.dream.min_hours, 6);
        assert_eq!(mem.dream.min_sessions, 10);
        assert_eq!(mem.dream.check_interval_secs, Some(300));
    });
}
#[test]
fn expands_multiple_vars_in_one_string() {
    with_env_var(
        "GROK_TEST_USER",
        "alice",
        || {
            with_env_var(
                "GROK_TEST_ROOT",
                "/a/b/c/d",
                || {
                    let toml_str = r#"
[mcp_servers.test]
command = "$GROK_TEST_USER $GROK_TEST_ROOT"
"#;
                    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
                    expand_env_vars_in_toml(&mut value);
                    let toml::Value::Table(table) = value else {
                        panic!("Expected table root");
                    };
                    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers")
                    else {
                        panic!("Expected mcp_servers table");
                    };
                    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
                        panic!("Expected test table");
                    };
                    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
                    assert_eq!(command, "alice /a/b/c/d");
                },
            );
        },
    );
}
#[test]
fn effective_half_life_temporal_decay_enabled() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: 14.0,
        },
        ..Default::default()
    };
    assert_eq!(config.effective_half_life_days(), Some(14.0));
}
#[test]
fn effective_half_life_temporal_decay_enabled_zero_disables() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: 0.0,
        },
        ..Default::default()
    };
    assert_eq!(
            config.effective_half_life_days(),
            None,
            "zero half_life_days should disable decay"
        );
}
#[test]
fn effective_half_life_temporal_decay_enabled_negative_disables() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: -5.0,
        },
        ..Default::default()
    };
    assert_eq!(
            config.effective_half_life_days(),
            None,
            "negative half_life_days should disable decay"
        );
}
#[test]
fn effective_half_life_disabled_default_recency_returns_none() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: DEFAULT_RECENCY_DECAY,
        ..Default::default()
    };
    assert_eq!(
            config.effective_half_life_days(),
            None,
            "disabled + default recency_decay should return None"
        );
}
#[test]
fn effective_half_life_disabled_legacy_recency_converts() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: 0.9,
        ..Default::default()
    };
    let half_life = config
        .effective_half_life_days()
        .expect("should convert legacy recency_decay=0.9");
    assert!(
            (half_life - 6.58).abs() < 0.1,
            "recency_decay=0.9 should convert to ~6.58 day half-life, got {half_life}"
        );
}
#[test]
fn effective_half_life_disabled_legacy_recency_098() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: 0.98,
        ..Default::default()
    };
    let half_life = config
        .effective_half_life_days()
        .expect("should convert legacy recency_decay=0.98");
    assert!(
            (half_life - 34.3).abs() < 0.5,
            "recency_decay=0.98 should convert to ~34.3 day half-life, got {half_life}"
        );
}
#[test]
fn effective_half_life_disabled_legacy_recency_out_of_range_ignored() {
    for bad_value in [0.0_f32, 1.0, -0.5, 1.5] {
        let config = MemorySearchConfig {
            temporal_decay: TemporalDecayConfig {
                enabled: false,
                half_life_days: 30.0,
            },
            recency_decay: bad_value,
            ..Default::default()
        };
        assert_eq!(
                config.effective_half_life_days(),
                None,
                "recency_decay={bad_value} should not convert"
            );
    }
}
#[test]
fn mmr_lambda_clamped_above_one() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.search.mmr]
enabled = true
lambda = 2.0
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.search.mmr.enabled);
        assert!(
                (mem.search.mmr.lambda - 1.0).abs() < f64::EPSILON,
                "lambda=2.0 should clamp to 1.0, got {}",
                mem.search.mmr.lambda
            );
    });
}
#[test]
fn mmr_lambda_clamped_below_zero() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.search.mmr]
enabled = true
lambda = -0.5
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.search.mmr.enabled);
        assert!(
                mem.search.mmr.lambda.abs() < f64::EPSILON,
                "lambda=-0.5 should clamp to 0.0, got {}",
                mem.search.mmr.lambda
            );
    });
}
#[test]
fn memory_config_remote_temporal_decay() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_temporal_decay_enabled: Some(false),
            memory_temporal_decay_half_life_days: Some(14.0),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(!mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON);
    });
}
#[test]
fn memory_config_remote_mmr() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_mmr_enabled: Some(true),
            memory_mmr_lambda: Some(0.5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.search.mmr.enabled);
        assert!((mem.search.mmr.lambda - 0.5).abs() < f64::EPSILON);
    });
}
#[test]
fn memory_config_remote_mmr_lambda_clamped() {
    without_grok_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_mmr_lambda: Some(5.0),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
                (mem.search.mmr.lambda - 1.0).abs() < f64::EPSILON,
                "remote mmr_lambda=5.0 should be clamped to 1.0"
            );
    });
}
#[test]
fn memory_config_partial_search_uses_remote_temporal_decay_and_mmr() {
    without_grok_memory(|| {
        let toml_str = r#"
[memory.search]
max_results = 8
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_temporal_decay_enabled: Some(false),
            memory_mmr_enabled: Some(true),
            memory_mmr_lambda: Some(0.3),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(!mem.search.temporal_decay.enabled);
        assert!(mem.search.mmr.enabled);
        assert!((mem.search.mmr.lambda - 0.3).abs() < f64::EPSILON);
    });
}
/// Mutex to serialize tests that touch the GROK_SUBAGENTS env var.
static SUBAGENTS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Run `f` with GROK_SUBAGENTS explicitly unset.
fn without_grok_subagents<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("GROK_SUBAGENTS", None, f)
}
/// Run `f` with GROK_SUBAGENTS set to a specific value.
fn with_grok_subagents<T>(value: &str, f: impl FnOnce() -> T) -> T {
    let _guard = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("GROK_SUBAGENTS", Some(value), f)
}
#[test]
fn subagents_config_default_enabled() {
    without_grok_subagents(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_max_depth_defaults_to_one() {
    assert_eq!(
            SubagentsConfig::resolve_max_depth(None, None, None),
            SubagentsConfig::DEFAULT_MAX_DEPTH
        );
    assert_eq!(SubagentsConfig::DEFAULT_MAX_DEPTH, 1);
}
#[test]
fn subagents_max_depth_env_beats_toml_and_remote() {
    assert_eq!(
            SubagentsConfig::resolve_max_depth(Some("3"), Some(2), Some(4)),
            3
        );
}
#[test]
fn subagents_max_depth_toml_beats_remote() {
    assert_eq!(
            SubagentsConfig::resolve_max_depth(None, Some(2), Some(4)),
            2
        );
}
#[test]
fn subagents_max_depth_remote_used_when_local_absent() {
    assert_eq!(SubagentsConfig::resolve_max_depth(None, None, Some(5)), 5);
}
#[test]
fn subagents_max_depth_clamps_below_one_to_one() {
    assert_eq!(SubagentsConfig::clamp_max_depth(-3, "test"), 1);
    assert_eq!(SubagentsConfig::clamp_max_depth(0, "test"), 1);
    assert_eq!(
            SubagentsConfig::resolve_max_depth(Some("-2"), None, None),
            1
        );
    assert_eq!(
            SubagentsConfig::resolve_max_depth(None, Some(0), Some(3)),
            1
        );
    assert_eq!(
            SubagentsConfig::resolve_max_depth(None, None, Some(0)),
            1
        );
}
#[test]
fn subagents_max_depth_invalid_env_falls_through() {
    assert_eq!(
            SubagentsConfig::resolve_max_depth(Some("not-a-number"), Some(2), None),
            2
        );
}
#[test]
fn subagents_config_parses_max_depth_from_toml() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nmax_depth = 2\n")
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert_eq!(sa.max_depth, Some(2));
    });
}
#[test]
fn subagent_limit_counts_resolve_env_over_toml_over_remote_over_default() {
    use xai_grok_tools::implementations::grok_build::task::admission;
    let resolve = SubagentsConfig::resolve_max_concurrent;
    assert_eq!(resolve(Some("3"), Some(2), Some(4)), 3);
    assert_eq!(resolve(None, Some(2), Some(4)), 2);
    assert_eq!(resolve(None, None, Some(5)), 5);
    assert_eq!(resolve(None, None, None), admission::DEFAULT_MAX_CONCURRENT);
    assert_eq!(resolve(Some("-2"), Some(2), None), 2);
    assert_eq!(resolve(None, Some(0), Some(3)), 1);
    assert_eq!(
            SubagentsConfig::resolve_workflow_max_concurrent(None, None, None),
            crate::session::workflow::host_service::DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS
        );
}
#[test]
fn subagent_sampling_limit_applies_precedence_and_clamps() {
    use crate::agent::subagent::MAX_SUBAGENT_SAMPLING_LIMIT;
    use xai_grok_tools::implementations::grok_build::task::admission::DEFAULT_MAX_CONCURRENT;
    let resolve = |env: Option<&str>, config: Option<i64>, remote: Option<u32>| SubagentsConfig::resolve_sampling_limit(
        env,
        config,
        remote,
        DEFAULT_MAX_CONCURRENT,
    );
    assert_eq!(resolve(None, None, None), DEFAULT_MAX_CONCURRENT);
    assert_eq!(resolve(Some("24"), Some(8), Some(4)), 24);
    for bad in ["0", "-1", "garbage"] {
        assert_eq!(resolve(Some(bad), None, None), DEFAULT_MAX_CONCURRENT);
    }
    let huge = (MAX_SUBAGENT_SAMPLING_LIMIT as u64 + 1_000).to_string();
    assert_eq!(resolve(Some(&huge), None, None), MAX_SUBAGENT_SAMPLING_LIMIT);
    assert_eq!(
            resolve(Some("999999999999999999999999999"), None, None),
            DEFAULT_MAX_CONCURRENT
        );
    assert!(resolve(Some("0"), Some(0), Some(0)) > 0);
}
#[test]
fn subagent_sampling_limit_env_override_beats_toml() {
    let _lock = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::env::EnvVarGuard::set(SubagentsConfig::ENV_SAMPLING_LIMIT, "24");
    let raw: toml::Value = toml::from_str("[subagents]\nsampling_limit = 8\n").unwrap();
    let mut config = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    config.resolve_subagents(false, &raw);
    assert_eq!(config.subagents_sampling_limit, 24);
}
#[test]
fn subagent_sampling_limit_defaults_to_resolved_subagents_max_concurrent() {
    let _lock = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = crate::env::EnvVarGuard::remove(SubagentsConfig::ENV_SAMPLING_LIMIT)
        .and_set(SubagentsConfig::ENV_MAX_CONCURRENT, "20");
    let raw: toml::Value = toml::from_str("[subagents]\n").unwrap();
    let mut config = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    config.resolve_subagents(false, &raw);
    assert_eq!(config.subagents_max_concurrent, 20);
    assert_eq!(
            config.subagents_sampling_limit,
            config.subagents_max_concurrent
        );
}
#[test]
fn subagent_limit_behavior_resolves_env_over_toml_over_remote_over_queue() {
    use xai_grok_tools::implementations::grok_build::task::admission::LimitBehavior;
    let resolve = SubagentsConfig::resolve_limit_behavior;
    assert_eq!(
            resolve(Some("fail"), Some("queue"), Some("queue")),
            LimitBehavior::Fail
        );
    assert_eq!(resolve(None, Some("FAIL"), Some("queue")), LimitBehavior::Fail);
    assert_eq!(resolve(None, None, Some("fail")), LimitBehavior::Fail);
    assert_eq!(resolve(None, None, None), LimitBehavior::Queue);
    assert_eq!(
            resolve(Some("sometimes"), Some("fail"), None),
            LimitBehavior::Fail
        );
    assert_eq!(resolve(None, Some("sometimes"), None), LimitBehavior::Queue);
}
#[test]
fn subagents_config_parses_limits_from_toml() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str(
                "[subagents]\nmax_concurrent = 4\nsampling_limit = 6\nlimit_behavior = \"fail\"\nworkflow_max_concurrent = 8\n",
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert_eq!(sa.max_concurrent, Some(4));
        assert_eq!(sa.sampling_limit, Some(6));
        assert_eq!(sa.limit_behavior.as_deref(), Some("fail"));
        assert_eq!(sa.workflow_max_concurrent, Some(8));
    });
}
#[test]
fn subagents_config_parses_negative_max_depth_without_dropping_section() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str(
                "[subagents]\nenabled = true\nmax_depth = -1\n",
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
        assert_eq!(sa.max_depth, Some(-1));
        assert_eq!(
                SubagentsConfig::resolve_max_depth(None, sa.max_depth, None),
                1
            );
    });
}
#[test]
fn subagents_config_cli_flag_enables() {
    without_grok_subagents(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(true, &config);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_env_var_enables() {
    with_grok_subagents(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(false, &config);
            assert!(sa.enabled);
        },
    );
}
#[test]
fn subagents_config_env_var_disables() {
    with_grok_subagents(
        "0",
        || {
            let config: toml::Value = toml::from_str("[subagents]\nenabled = true")
                .unwrap();
            let sa = SubagentsConfig::resolve(false, &config);
            assert!(!sa.enabled, "GROK_SUBAGENTS=0 should override config file");
        },
    );
}
#[test]
fn subagents_config_toml_enables() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_local_disabled_wins() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = false")
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(!sa.enabled, "local [subagents] enabled=false should win");
    });
}
#[test]
fn subagents_config_env_var_disables_default() {
    with_grok_subagents(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(false, &config);
            assert!(
                !sa.enabled,
                "GROK_SUBAGENTS=0 should override the enabled default"
            );
        },
    );
}
/// A `subagents_enabled` key served by an old cli-chat-proxy must parse as an unknown key and have no effect on resolution.
#[test]
fn subagents_config_remote_settings_key_is_ignored() {
    without_grok_subagents(|| {
        let _settings: crate::util::config::RemoteSettings = serde_json::from_str(
                r#"{"subagents_enabled": false}"#,
            )
            .expect("unknown subagents_enabled key must not break parsing");
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_cli_flag_overrides_env_var() {
    with_grok_subagents(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(true, &config);
            assert!(
                sa.enabled,
                "--subagents CLI flag should override GROK_SUBAGENTS=0"
            );
        },
    );
}
#[test]
fn subagents_config_models_parsed() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents]
                enabled = true

                [subagents.models]
                explore = "grok-3-fast"
                plan = "grok-4.5"
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
        assert_eq!(sa.models.len(), 2);
        assert_eq!(sa.models.get("explore").unwrap(), "grok-3-fast");
        assert_eq!(sa.models.get("plan").unwrap(), "grok-4.5");
    });
}
#[test]
fn subagents_config_models_empty_when_missing() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
        assert!(sa.models.is_empty());
    });
}
#[test]
fn subagents_config_models_without_enabled() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents.models]
                explore = "grok-3-fast"
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(
                !sa.enabled,
                "explicit [subagents] section without enabled should be false"
            );
        assert_eq!(sa.models.len(), 1);
        assert_eq!(sa.models.get("explore").unwrap(), "grok-3-fast");
    });
}
#[test]
fn subagents_config_models_with_env_var_enables() {
    with_grok_subagents(
        "1",
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [subagents.models]
                explore = "grok-3-fast"
                "#,
                )
                .unwrap();
            let sa = SubagentsConfig::resolve(false, &config);
            assert!(sa.enabled, "GROK_SUBAGENTS=1 should enable");
            assert_eq!(sa.models.get("explore").unwrap(), "grok-3-fast");
        },
    );
}
#[test]
fn subagents_config_toggle_mixed_values() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents]
                enabled = true

                [subagents.toggle]
                explore = true
                plan = false
                general-purpose = true
                code-reviewer = false
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
        assert_eq!(sa.toggle.len(), 4);
        assert_eq!(sa.toggle.get("explore").copied(), Some(true));
        assert_eq!(sa.toggle.get("plan").copied(), Some(false));
        assert_eq!(sa.toggle.get("general-purpose").copied(), Some(true));
        assert_eq!(sa.toggle.get("code-reviewer").copied(), Some(false));
    });
}
#[test]
fn subagents_config_toggle_missing_defaults_to_empty() {
    without_grok_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config);
        assert!(sa.enabled);
        assert!(
                sa.toggle.is_empty(),
                "missing [subagents.toggle] should produce empty HashMap"
            );
    });
}
#[test]
fn subagents_config_is_subagent_enabled_absent_defaults_true() {
    let sa = SubagentsConfig {
        enabled: true,
        toggle: std::collections::HashMap::from([("plan".to_string(), false)]),
        ..Default::default()
    };
    assert!(
            sa.is_subagent_enabled("explore"),
            "absent key should default to enabled (true)"
        );
    assert!(
            sa.is_subagent_enabled("general-purpose"),
            "absent key should default to enabled (true)"
        );
}
#[test]
fn subagents_config_is_subagent_enabled_false_when_toggled_off() {
    let sa = SubagentsConfig {
        enabled: true,
        toggle: std::collections::HashMap::from([
            ("plan".to_string(), false),
            ("code-reviewer".to_string(), false),
            ("explore".to_string(), true),
        ]),
        ..Default::default()
    };
    assert!(
            !sa.is_subagent_enabled("plan"),
            "plan = false should return disabled"
        );
    assert!(
            !sa.is_subagent_enabled("code-reviewer"),
            "code-reviewer = false should return disabled"
        );
    assert!(
            sa.is_subagent_enabled("explore"),
            "explore = true should return enabled"
        );
}
fn with_managed_mcp_env<T>(
    managed_mcps: Option<&str>,
    gateway_tools: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt(
        "GROK_MANAGED_MCPS_ENABLED",
        managed_mcps,
        || with_env_var_opt("GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED", gateway_tools, f),
    )
}
#[test]
#[serial_test::serial]
fn managed_mcps_interactive_default_enabled() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ManagedMcpsConfig::resolve(&empty, None, false);
            assert!(cfg.enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcps_headless_default_disabled() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ManagedMcpsConfig::resolve(&empty, None, true);
            assert!(!cfg.enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_default_disabled() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ManagedMcpsConfig::resolve(&empty, None, false);
            assert!(!cfg.gateway_tools_enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_require_managed_master() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [managed_mcps]
                gateway_tools_enabled = true
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                managed_mcps_enabled: Some(false),
                ..Default::default()
            };
            let cfg = ManagedMcpsConfig::resolve(&config, Some(&remote), true);
            assert!(!cfg.enabled);
            assert!(!cfg.gateway_tools_enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_remote_enabled() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                managed_mcp_gateway_tools_enabled: Some(true),
                ..Default::default()
            };
            let cfg = ManagedMcpsConfig::resolve(&empty, Some(&remote), false);
            assert!(cfg.gateway_tools_enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_env_overrides_remote() {
    with_managed_mcp_env(
        None,
        Some("0"),
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                managed_mcp_gateway_tools_enabled: Some(true),
                ..Default::default()
            };
            let cfg = ManagedMcpsConfig::resolve(&empty, Some(&remote), false);
            assert!(!cfg.gateway_tools_enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_env_on_overrides_remote_off() {
    with_managed_mcp_env(
        None,
        Some("1"),
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                managed_mcp_gateway_tools_enabled: Some(false),
                ..Default::default()
            };
            let cfg = ManagedMcpsConfig::resolve(&empty, Some(&remote), false);
            assert!(cfg.gateway_tools_enabled);
        },
    );
}
#[test]
#[serial_test::serial]
fn managed_mcp_gateway_tools_enabled_with_managed_master() {
    with_managed_mcp_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [managed_mcps]
                enabled = true
                gateway_tools_enabled = true
                "#,
                )
                .unwrap();
            let cfg = ManagedMcpsConfig::resolve(&config, None, false);
            assert!(cfg.enabled);
            assert!(cfg.gateway_tools_enabled);
        },
    );
}
fn with_model_overrides_env_full<T>(
    ws: Option<&str>,
    ss: Option<&str>,
    id: Option<&str>,
    ps: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt(
        "GROK_WEB_SEARCH_MODEL",
        ws,
        || with_env_var_opt(
            "GROK_SESSION_SUMMARY_MODEL",
            ss,
            || with_env_var_opt(
                "GROK_IMAGE_DESCRIPTION_MODEL",
                id,
                || with_env_var_opt("GROK_PROMPT_SUGGESTIONS_MODEL", ps, f),
            ),
        ),
    )
}
fn with_model_overrides_env<T>(
    ws: Option<&str>,
    ss: Option<&str>,
    id: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    with_model_overrides_env_full(ws, ss, id, None, f)
}
#[test]
fn model_overrides_remote_settings_blocked_by_local_config() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                web_search = "local-ws"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                web_search_model: Some("remote-ws".to_owned()),
                session_summary_model: Some("remote-ss".to_owned()),
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(cfg.web_search, "local-ws");
            assert_eq!(cfg.session_summary, Some("remote-ss".to_owned()));
            assert_eq!(cfg.image_description, Some("remote-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_cli_overrides_everything() {
    with_model_overrides_env(
        Some("env-ws"),
        Some("env-ss"),
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                web_search = "local-ws"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(
                Some("cli-ws"),
                Some("cli-ss"),
                &config,
                None,
            );
            assert_eq!(cfg.web_search, "cli-ws");
            assert_eq!(cfg.session_summary, Some("cli-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_remote_settings_applies_without_local_config() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                web_search_model: Some("remote-ws".to_owned()),
                session_summary_model: Some("remote-ss".to_owned()),
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(cfg.web_search, "remote-ws");
            assert_eq!(cfg.session_summary, Some("remote-ss".to_owned()));
            assert_eq!(cfg.image_description, Some("remote-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_local_image_description_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = "local-id"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(cfg.image_description, Some("local-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_default_image_description_is_grok_build() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, None);
            assert_eq!(
                cfg.image_description,
                Some(crate::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_default_session_summary_is_grok_build() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, None);
            assert_eq!(
                cfg.session_summary,
                Some(crate::models::default_session_summary_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_local_session_summary_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(cfg.session_summary, Some("local-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_session_summary_overrides_remote() {
    with_model_overrides_env(
        None,
        Some("env-ss"),
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(cfg.session_summary, Some("env-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_session_summary_overrides_local() {
    with_model_overrides_env(
        None,
        Some("env-ss"),
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, None, &config, None);
            assert_eq!(cfg.session_summary, Some("env-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_session_summary_toml_uses_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = ""
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, None, &config, None);
            assert_eq!(
                cfg.session_summary,
                Some(crate::models::default_session_summary_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_empty_session_summary_remote_uses_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("   ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(
                cfg.session_summary,
                Some(crate::models::default_session_summary_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_cli_session_summary_overrides_everything() {
    with_model_overrides_env(
        None,
        Some("env-ss"),
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(
                None,
                Some("cli-ss"),
                &config,
                Some(&remote),
            );
            assert_eq!(cfg.session_summary, Some("cli-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_cli_session_summary_uses_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, Some(""), &empty, None);
            assert_eq!(
                cfg.session_summary,
                Some(crate::models::default_session_summary_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_env_image_description_overrides_remote() {
    with_model_overrides_env(
        None,
        None,
        Some("env-id"),
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(cfg.image_description, Some("env-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_image_description_overrides_local() {
    with_model_overrides_env(
        None,
        None,
        Some("env-id"),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = "local-id"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, None, &config, None);
            assert_eq!(cfg.image_description, Some("env-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_image_description_toml_uses_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = ""
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, None, &config, None);
            assert_eq!(
                cfg.image_description,
                Some(crate::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_empty_image_description_remote_uses_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("   ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(
                cfg.image_description,
                Some(crate::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_unpinned_by_default() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, None);
            assert_eq!(cfg.prompt_suggestion, PromptSuggestModelPin::Unpinned);
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_local_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion,
                PromptSuggestModelPin::Pinned("local-ps".to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_remote_applies_without_local() {
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &empty, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion,
                PromptSuggestModelPin::Pinned("remote-ps".to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_env_wins_over_local_and_remote() {
    with_model_overrides_env_full(
        None,
        None,
        None,
        Some("env-ps"),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion,
                PromptSuggestModelPin::Env("env-ps".to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_blank_values_are_unset() {
    with_model_overrides_env_full(
        None,
        None,
        None,
        Some("  "),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, None, &config, None);
            assert_eq!(
                cfg.prompt_suggestion,
                PromptSuggestModelPin::Pinned("local-ps".to_owned())
            );
        },
    );
    with_model_overrides_env(
        None,
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "   "
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("  ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, None, &config, Some(&remote));
            assert_eq!(cfg.prompt_suggestion, PromptSuggestModelPin::Unpinned);
        },
    );
}
/// Lock shared by every test that touches the env vars read by `ToolsConfig::resolve`, so tests across both fields can't race.
static TOOLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Set both `ToolsConfig` env vars for the duration of `f`, then restore.
/// `None` clears the var.
fn with_tools_env<T>(
    respect_gitignore: Option<&str>,
    disable_zdr: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    let _guard = TOOLS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt(
        "GROK_RESPECT_GITIGNORE",
        respect_gitignore,
        || with_env_var_opt("GROK_DISABLE_ZDR_INCOMPATIBLE_TOOLS", disable_zdr, f),
    )
}
fn without_grok_respect_gitignore<T>(f: impl FnOnce() -> T) -> T {
    with_tools_env(None, None, f)
}
fn with_grok_respect_gitignore<T>(value: &str, f: impl FnOnce() -> T) -> T {
    with_tools_env(Some(value), None, f)
}
#[test]
fn tools_config_default_disabled() {
    without_grok_respect_gitignore(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let tc = ToolsConfig::resolve(&config);
        assert!(!tc.respect_gitignore);
    });
}
#[test]
fn tools_config_toml_disables() {
    without_grok_respect_gitignore(|| {
        let config: toml::Value = toml::from_str("[tools]\nrespect_gitignore = false")
            .unwrap();
        let tc = ToolsConfig::resolve(&config);
        assert!(!tc.respect_gitignore);
    });
}
#[test]
fn tools_config_env_var_disables() {
    with_grok_respect_gitignore(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let tc = ToolsConfig::resolve(&config);
            assert!(!tc.respect_gitignore);
        },
    );
}
#[test]
fn tools_config_env_var_overrides_toml() {
    with_grok_respect_gitignore(
        "1",
        || {
            let config: toml::Value = toml::from_str(
                    "[tools]\nrespect_gitignore = false",
                )
                .unwrap();
            let tc = ToolsConfig::resolve(&config);
            assert!(tc.respect_gitignore, "env var should override config file");
        },
    );
}
#[test]
fn tools_config_env_false_overrides_toml_true() {
    with_grok_respect_gitignore(
        "false",
        || {
            let config: toml::Value = toml::from_str("[tools]\nrespect_gitignore = true")
                .unwrap();
            let tc = ToolsConfig::resolve(&config);
            assert!(
                !tc.respect_gitignore,
                "GROK_RESPECT_GITIGNORE=false should override config file"
            );
        },
    );
}
#[test]
fn zdr_incompatible_tools_env_overrides_toml_false() {
    with_tools_env(
        None,
        Some("true"),
        || {
            let config: toml::Value = toml::from_str(
                    "[tools]\ndisable_zdr_incompatible_tools = false",
                )
                .unwrap();
            let tc = ToolsConfig::resolve(&config);
            assert!(tc.disable_zdr_incompatible_tools, "env must override TOML");
        },
    );
}
#[test]
fn zdr_video_output_s3_deserializes_from_tools_block() {
    let config: toml::Value = toml::from_str(
            r#"
            [tools]
            disable_zdr_incompatible_tools = true

            [tools.zdr_video_output_s3]
            bucket = "team-videos"
            endpoint = "https://s3.example.com"
            region = "us-east-1"

            [tools.zdr_video_output_s3.read_write]
            access_key_id = "AKIA..."
            secret_access_key = "secret"
            "#,
        )
        .unwrap();
    let tc = ToolsConfig::resolve(&config);
    let s3 = tc.zdr_video_output_s3.expect("zdr_video_output_s3 should deserialize");
    assert_eq!(s3.bucket, "team-videos");
    assert!(s3.is_valid());
}
#[test]
fn incomplete_zdr_video_output_s3_is_ignored() {
    without_grok_respect_gitignore(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [tools]
                disable_zdr_incompatible_tools = true

                [tools.zdr_video_output_s3]
                bucket = "team-videos"
                "#,
            )
            .unwrap();
        let tc = ToolsConfig::resolve(&config);
        assert!(tc.zdr_video_output_s3.is_none());
        assert!(
                tc.disable_zdr_incompatible_tools,
                "incomplete zdr_video_output_s3 must not drop disable_zdr_incompatible_tools"
            );
    });
}
#[test]
fn malformed_zdr_video_output_s3_preserves_zdr_flag() {
    without_grok_respect_gitignore(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [tools]
                disable_zdr_incompatible_tools = true
                respect_gitignore = true

                [tools.zdr_video_output_s3]
                bucket = "team-videos"
                endpoint = "https://s3.example.com"
                region = "us-east-1"
                "#,
            )
            .unwrap();
        let tc = ToolsConfig::resolve(&config);
        assert!(tc.zdr_video_output_s3.is_none());
        assert!(tc.disable_zdr_incompatible_tools);
        assert!(tc.respect_gitignore);
    });
}
#[test]
fn media_gen_caps_resolve_env_over_toml_over_remote_over_default() {
    use xai_grok_tools::media_gen_limits::{
        DEFAULT_MAX_PARALLEL_IMAGE_GEN, DEFAULT_MAX_PARALLEL_VIDEO_GEN,
    };
    let config: toml::Value = toml::from_str(
            "[tools.media_gen]\nmax_parallel_image_gen_calls = 2\nmax_parallel_video_gen_calls = 1\n",
        )
        .unwrap();
    let tc = ToolsConfig::resolve(&config);
    assert_eq!(tc.media_gen.max_parallel_image_gen_calls, Some(2));
    assert_eq!(tc.media_gen.max_parallel_video_gen_calls, Some(1));
    let image = ToolsConfig::resolve_max_parallel_image_gen_calls;
    let video = ToolsConfig::resolve_max_parallel_video_gen_calls;
    assert_eq!(image(Some("3"), Some(2), Some(4)), 3);
    assert_eq!(image(None, Some(2), Some(4)), 2);
    assert_eq!(image(None, None, Some(5)), 5);
    assert_eq!(image(None, None, None), DEFAULT_MAX_PARALLEL_IMAGE_GEN);
    assert_eq!(image(Some("not-a-number"), Some(2), None), 2);
    assert_eq!(image(Some("0"), Some(2), None), 1);
    assert_eq!(image(None, Some(0), Some(3)), 1);
    assert_eq!(video(None, None, None), DEFAULT_MAX_PARALLEL_VIDEO_GEN);
    assert_eq!(video(Some("9"), Some(1), Some(2)), 9);
}
#[test]
fn roles_parse_from_toml() {
    let toml_str = r#"
            [roles.researcher]
            description = "Deep research agent"
            default_capability_mode = "read-only"
            model = "grok-3"

            [roles.implementer]
            description = "Implementation agent"
            default_capability_mode = "all"
            prompt_file = ".grok/prompts/impl.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.roles.len(), 2);
    let researcher = cfg.get_role("researcher").unwrap();
    assert_eq!(researcher.description, "Deep research agent");
    assert_eq!(
            researcher.default_capability_mode.as_deref(),
            Some("read-only")
        );
    assert_eq!(researcher.model.as_deref(), Some("grok-3"));
    assert!(researcher.prompt_file.is_none());
    let implementer = cfg.get_role("implementer").unwrap();
    assert_eq!(implementer.description, "Implementation agent");
    assert_eq!(implementer.default_capability_mode.as_deref(), Some("all"));
    assert!(implementer.model.is_none());
    assert_eq!(
            implementer.prompt_file.as_deref(),
            Some(".grok/prompts/impl.md")
        );
}
#[test]
fn roles_default_to_empty() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.roles.is_empty());
}
#[test]
fn role_lookup_returns_none_for_unknown() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.get_role("nonexistent").is_none());
}
#[test]
fn role_minimal_config() {
    let toml_str = r#"
            [roles.simple]
            description = "A simple role"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let role = cfg.get_role("simple").unwrap();
    assert_eq!(role.description, "A simple role");
    assert!(role.default_capability_mode.is_none());
    assert!(role.model.is_none());
    assert!(role.prompt_file.is_none());
}
#[test]
fn validate_roles_catches_empty_description() {
    let toml_str = r#"
            [roles.bad]
            default_capability_mode = "read-only"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    let Some((name, msg)) = errors.first() else {
        panic!("expected one role error: {errors:?}");
    };
    assert_eq!(name.as_str(), "bad");
    assert!(msg.contains("description is required"));
}
#[test]
fn validate_roles_catches_invalid_capability_mode() {
    let toml_str = r#"
            [roles.bad]
            description = "Has invalid mode"
            default_capability_mode = "readonly"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    let Some((_, msg)) = errors.first() else {
        panic!("expected one role error: {errors:?}");
    };
    assert!(msg.contains("invalid default_capability_mode"));
    assert!(msg.contains("readonly"));
}
#[test]
fn validate_roles_passes_valid_config() {
    let toml_str = r#"
            [roles.good]
            description = "Valid role"
            default_capability_mode = "read-write"
            model = "grok-3"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.validate_roles().is_empty());
}
#[test]
fn validate_roles_catches_empty_prompt_file() {
    let toml_str = r#"
            [roles.bad]
            description = "Has blank prompt_file"
            prompt_file = "  "
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    let Some((_, msg)) = errors.first() else {
        panic!("expected one role error: {errors:?}");
    };
    assert!(msg.contains("prompt_file must not be empty"));
}
#[test]
fn validate_roles_accepts_valid_prompt_file() {
    let toml_str = r#"
            [roles.ok]
            description = "Valid prompt file"
            prompt_file = ".grok/prompts/ok.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.validate_roles().is_empty());
}
#[test]
fn discover_roles_loads_from_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".grok").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(
            roles_dir.join("reviewer.toml"),
            r#"
                description = "Code reviewer"
                default_capability_mode = "read-only"
            "#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    let role = cfg.get_role("reviewer").unwrap();
    assert_eq!(role.description, "Code reviewer");
    assert_eq!(role.default_capability_mode.as_deref(), Some("read-only"));
}
#[test]
fn discover_roles_inline_takes_precedence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".grok").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(
            roles_dir.join("researcher.toml"),
            r#"description = "File-based researcher""#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str(
            r#"
            [roles.researcher]
            description = "Inline researcher"
        "#,
        )
        .unwrap();
    cfg.discover_roles(tmp.path());
    let role = cfg.get_role("researcher").unwrap();
    assert_eq!(
            role.description, "Inline researcher",
            "inline config should take precedence over file"
        );
}
#[test]
fn discover_roles_ignores_non_toml_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".grok").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(roles_dir.join("readme.md"), "This is not a role definition")
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    assert!(cfg.roles.is_empty());
}
#[test]
fn discover_roles_skips_missing_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    assert!(cfg.roles.is_empty());
}
#[test]
fn personas_parse_from_toml() {
    let toml_str = r#"
            [personas.researcher]
            instructions = "You are a thorough researcher."

            [personas.concise]
            instructions = "Be concise."
            instructions_file = ".grok/personas/concise.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.personas.len(), 2);
    let researcher = cfg.get_persona("researcher").unwrap();
    assert_eq!(
            researcher.instructions.as_deref(),
            Some("You are a thorough researcher.")
        );
    assert!(researcher.instructions_file.is_none());
    let concise = cfg.get_persona("concise").unwrap();
    assert_eq!(concise.instructions.as_deref(), Some("Be concise."));
    assert_eq!(
            concise.instructions_file.as_deref(),
            Some(".grok/personas/concise.md")
        );
}
#[test]
fn personas_default_to_empty() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.personas.is_empty());
}
#[test]
fn persona_lookup_returns_none_for_unknown() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.get_persona("nonexistent").is_none());
}
#[test]
fn discover_personas_loads_from_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(".grok").join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("friendly.toml"),
            r#"instructions = "Be friendly and warm.""#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_personas(tmp.path());
    let p = cfg.get_persona("friendly").unwrap();
    assert_eq!(p.instructions.as_deref(), Some("Be friendly and warm."));
}
#[test]
fn discover_personas_inline_takes_precedence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(".grok").join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("strict.toml"), r#"instructions = "File-based strict""#)
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str(
            r#"
            [personas.strict]
            instructions = "Inline strict"
        "#,
        )
        .unwrap();
    cfg.discover_personas(tmp.path());
    assert_eq!(
            cfg.get_persona("strict").unwrap().instructions.as_deref(),
            Some("Inline strict"),
        );
}
fn write_subagent_definitions(root: &std::path::Path, definitions: &[(&str, &str)]) {
    let roles = root.join("roles");
    let personas = root.join("personas");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::create_dir_all(&personas).unwrap();
    for (name, source) in definitions {
        std::fs::write(
                roles.join(format!("{name}.toml")),
                format!("description = \"{source} role\""),
            )
            .unwrap();
        std::fs::write(
                personas.join(format!("{name}.toml")),
                format!("instructions = \"{source} persona\""),
            )
            .unwrap();
    }
}
#[test]
fn project_overlay_preserves_source_precedence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let home = tmp.path().join("home");
    let bundled = tmp.path().join("bundled");
    write_subagent_definitions(
        &project.join(".grok"),
        &[
            ("shadowed", "Project"),
            ("bundled-shadowed", "Project"),
            ("inline", "Project"),
            ("project-only", "Project"),
        ],
    );
    write_subagent_definitions(
        &home.join(".grok"),
        &[("shadowed", "User"), ("user-only", "User")],
    );
    write_subagent_definitions(
        &bundled,
        &[("bundled-shadowed", "Bundled"), ("bundled-only", "Bundled")],
    );
    let config = toml::from_str::<
        toml::Value,
    >(
            r#"
            [subagents]
            enabled = true

            [subagents.roles.inline]
            description = "Inline role"

            [subagents.personas.inline]
            instructions = "Inline persona"
            "#,
        )
        .unwrap();
    let base = SubagentsConfig::resolve_base_with_sources(
        false,
        &config,
        Some(&home.join(".grok")),
        &bundled,
    );
    let resolve = |project_trusted| {
        let (roles, personas) = SubagentsConfig::effective_definition_maps(
            &base.roles,
            &base.personas,
            &project,
            project_trusted,
        );
        SubagentsConfig {
            roles,
            personas,
            ..Default::default()
        }
    };
    let untrusted = resolve(false);
    assert_eq!(
            untrusted.get_role("shadowed").unwrap().description,
            "User role"
        );
    assert_eq!(
            untrusted
                .get_persona("shadowed")
                .and_then(|persona| persona.instructions.as_deref()),
            Some("User persona")
        );
    assert!(untrusted.get_role("project-only").is_none());
    assert!(untrusted.get_persona("project-only").is_none());
    assert!(untrusted.get_role("user-only").is_some());
    assert!(untrusted.get_persona("user-only").is_some());
    assert!(untrusted.get_role("bundled-only").is_some());
    assert!(untrusted.get_persona("bundled-only").is_some());
    assert_eq!(
            untrusted.get_role("bundled-shadowed").unwrap().description,
            "Bundled role"
        );
    assert_eq!(
            untrusted
                .get_persona("bundled-shadowed")
                .and_then(|persona| persona.instructions.as_deref()),
            Some("Bundled persona")
        );
    let trusted = resolve(true);
    assert_eq!(
            trusted.get_role("shadowed").unwrap().description,
            "Project role"
        );
    assert_eq!(
            trusted
                .get_persona("shadowed")
                .and_then(|persona| persona.instructions.as_deref()),
            Some("Project persona")
        );
    assert_eq!(
            trusted.get_role("bundled-shadowed").unwrap().description,
            "Project role"
        );
    assert_eq!(
            trusted
                .get_persona("bundled-shadowed")
                .and_then(|persona| persona.instructions.as_deref()),
            Some("Project persona")
        );
    assert_eq!(
            trusted.get_role("inline").unwrap().description,
            "Inline role"
        );
    assert_eq!(
            trusted
                .get_persona("inline")
                .and_then(|persona| persona.instructions.as_deref()),
            Some("Inline persona")
        );
    let denied_again = resolve(false);
    assert_eq!(
            denied_again.get_role("shadowed").unwrap().description,
            "User role"
        );
    assert!(denied_again.get_role("project-only").is_none());
}
#[test]
fn bundled_personas_and_roles_have_lowest_priority_in_resolve_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("workspace");
    let bundled = home.join(".grok").join("bundled");
    std::fs::create_dir_all(workspace.join(".grok").join("roles")).unwrap();
    std::fs::create_dir_all(workspace.join(".grok").join("personas")).unwrap();
    std::fs::create_dir_all(home.join(".grok").join("roles")).unwrap();
    std::fs::create_dir_all(home.join(".grok").join("personas")).unwrap();
    std::fs::create_dir_all(bundled.join("roles")).unwrap();
    std::fs::create_dir_all(bundled.join("personas")).unwrap();
    std::fs::write(
            bundled.join("roles/reviewer.toml"),
            r#"description = "Bundled reviewer""#,
        )
        .unwrap();
    std::fs::write(
            bundled.join("personas/reviewer.toml"),
            r#"instructions = "Bundled persona""#,
        )
        .unwrap();
    std::fs::write(
            home.join(".grok/roles/reviewer.toml"),
            r#"description = "User reviewer""#,
        )
        .unwrap();
    std::fs::write(
            home.join(".grok/personas/reviewer.toml"),
            r#"instructions = "User persona""#,
        )
        .unwrap();
    std::fs::write(
            workspace.join(".grok/roles/reviewer.toml"),
            r#"description = "Project reviewer""#,
        )
        .unwrap();
    std::fs::write(
            workspace.join(".grok/personas/reviewer.toml"),
            r#"instructions = "Project persona""#,
        )
        .unwrap();
    let config = toml::from_str::<
        toml::Value,
    >(
            r#"
            [subagents]
            enabled = true

            [subagents.roles.reviewer]
            description = "Inline reviewer"

            [subagents.personas.reviewer]
            instructions = "Inline persona"
            "#,
        )
        .unwrap();
    let base = SubagentsConfig::resolve_base_with_sources(
        true,
        &config,
        Some(&home.join(".grok")),
        &bundled,
    );
    let (roles, personas) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        &workspace,
        true,
    );
    let resolved = SubagentsConfig {
        roles,
        personas,
        ..Default::default()
    };
    assert_eq!(
            resolved.get_role("reviewer").unwrap().description,
            "Inline reviewer"
        );
    assert_eq!(
            resolved
                .get_persona("reviewer")
                .unwrap()
                .instructions
                .as_deref(),
            Some("Inline persona")
        );
    std::fs::remove_file(workspace.join(".grok/roles/reviewer.toml")).unwrap();
    std::fs::remove_file(workspace.join(".grok/personas/reviewer.toml")).unwrap();
    let config = toml::from_str::<
        toml::Value,
    >(r#"
            [subagents]
            enabled = true
            "#)
        .unwrap();
    let base = SubagentsConfig::resolve_base_with_sources(
        true,
        &config,
        Some(&home.join(".grok")),
        &bundled,
    );
    let (roles, personas) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        &workspace,
        true,
    );
    let resolved = SubagentsConfig {
        roles,
        personas,
        ..Default::default()
    };
    assert_eq!(
            resolved.get_role("reviewer").unwrap().description,
            "User reviewer"
        );
    assert_eq!(
            resolved
                .get_persona("reviewer")
                .unwrap()
                .instructions
                .as_deref(),
            Some("User persona")
        );
    std::fs::remove_file(home.join(".grok/roles/reviewer.toml")).unwrap();
    std::fs::remove_file(home.join(".grok/personas/reviewer.toml")).unwrap();
    let config = toml::from_str::<
        toml::Value,
    >(r#"
            [subagents]
            enabled = true
            "#)
        .unwrap();
    let base = SubagentsConfig::resolve_base_with_sources(
        true,
        &config,
        Some(&home.join(".grok")),
        &bundled,
    );
    let (roles, personas) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        &workspace,
        true,
    );
    let resolved = SubagentsConfig {
        roles,
        personas,
        ..Default::default()
    };
    assert_eq!(
            resolved.get_role("reviewer").unwrap().description,
            "Bundled reviewer"
        );
    assert_eq!(
            resolved
                .get_persona("reviewer")
                .unwrap()
                .instructions
                .as_deref(),
            Some("Bundled persona")
        );
}
#[test]
fn render_io_summary_shows_bundled_for_bundled_personas() {
    let persona = SubagentPersona {
        instructions: Some("Bundled instructions".to_string()),
        source_path: Some("/tmp/home/.grok/bundled/personas/reviewer.toml".to_string()),
        ..Default::default()
    };
    let summary = persona.render_io_summary("reviewer");
    assert!(summary.contains("[bundled]"));
}
#[test]
fn roles_coexist_with_models_and_toggle() {
    let toml_str = r#"
            enabled = true
            [models]
            explore = "grok-fast"
            [toggle]
            plan = false
            [roles.researcher]
            description = "Research agent"
            default_capability_mode = "read-only"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.enabled);
    assert_eq!(
            cfg.models.get("explore").map(|s| s.as_str()),
            Some("grok-fast")
        );
    assert!(!cfg.is_subagent_enabled("plan"));
    assert!(cfg.get_role("researcher").is_some());
}
#[test]
fn add_hooks_path_appends() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/some/path", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(content.contains("/some/path"));
}
#[test]
fn add_hooks_path_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/dup/path", &paths_file);
    let _ = add_hooks_path_to_file("/dup/path", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    let count = content.lines().filter(|l| l.trim() == "/dup/path").count();
    assert_eq!(count, 1);
}
#[test]
fn remove_hooks_path_removes() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/to/remove", &paths_file);
    let removed = remove_hooks_path_from_file("/to/remove", &paths_file).unwrap();
    assert!(removed);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(!content.contains("/to/remove"));
}
/// Unregistered paths report `false` so callers refuse instead of claiming a removal that never happened.
#[test]
fn remove_hooks_path_reports_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let removed = remove_hooks_path_from_file("/nonexistent/path", &paths_file).unwrap();
    assert!(!removed);
}
#[test]
fn remove_hooks_path_preserves_others() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/keep/me", &paths_file);
    let _ = add_hooks_path_to_file("/remove/me", &paths_file);
    let _ = add_hooks_path_to_file("/keep/me/too", &paths_file);
    let _ = remove_hooks_path_from_file("/remove/me", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(content.contains("/keep/me"));
    assert!(content.contains("/keep/me/too"));
    assert!(!content.contains("/remove/me"));
}
#[test]
fn add_hooks_path_succeeds_on_first_add() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let result = add_hooks_path_to_file("/first", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn add_hooks_path_succeeds_on_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/dup", &paths_file);
    let result = add_hooks_path_to_file("/dup", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn remove_hooks_path_succeeds_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/present", &paths_file);
    let result = remove_hooks_path_from_file("/present", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn remove_hooks_path_succeeds_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let result = remove_hooks_path_from_file("/missing", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn add_dismissed_plugin_cta_creates_table() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("[plugin_cta]"));
    assert!(content.contains("figma"));
    assert!(dismissed_plugin_ctas_in_file(&config_path).contains("figma"));
}
#[test]
fn add_dismissed_plugin_cta_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    add_dismissed_plugin_cta_to_file("notion", &config_path).unwrap();
    add_dismissed_plugin_cta_to_file("notion", &config_path).unwrap();
    let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(&config_path).unwrap(),
        )
        .unwrap();
    let dismissed = config
        .get("plugin_cta")
        .and_then(|v| v.get("dismissed"))
        .and_then(|v| v.as_array())
        .unwrap();
    let count = dismissed.iter().filter(|v| v.as_str() == Some("notion")).count();
    assert_eq!(count, 1);
}
#[test]
fn dismissed_plugin_ctas_reflects_added_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    assert!(!dismissed_plugin_ctas_in_file(&config_path).contains("figma"));
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let dismissed = dismissed_plugin_ctas_in_file(&config_path);
    assert!(dismissed.contains("figma"));
    assert!(!dismissed.contains("notion"));
}
#[test]
fn add_dismissed_plugin_cta_preserves_other_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "[plugins]\ndisabled = [\"keep-me\"]\n").unwrap();
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(&config_path).unwrap(),
        )
        .unwrap();
    assert_eq!(
            config
                .get("plugins")
                .and_then(|v| v.get("disabled"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("keep-me"),
        );
    assert!(dismissed_plugin_ctas_in_file(&config_path).contains("figma"));
}
#[test]
fn config_layers_user_overrides_managed() {
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\ntelemetry = false\n").unwrap(),
        user: toml::from_str("[features]\ntelemetry = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(
            Some(crate::agent::config::TelemetryMode::Enabled),
            cfg.features.telemetry
        );
}
/// A provider in a trusted disk layer resolves through the real `ConfigLayers` and `effective_config_disk_only` parse path.
/// The direct-TOML parse tests bypass that path.
/// (`ConfigLayers` has no project slot, so a repo `.grok/config.toml` structurally cannot supply one.)
#[test]
fn auth_provider_honored_only_from_trusted_disk_layers() {
    let layers = ConfigLayers {
        managed: toml::from_str(
                "[auth_provider.corp]\ncommand = \"/usr/local/bin/corp-token\"\n",
            )
            .unwrap(),
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(
            cfg.auth_providers.get("corp").map(|c| c.command.as_str()),
            Some("/usr/local/bin/corp-token"),
            "a provider in a trusted disk layer is honored"
        );
}
#[test]
fn model_provider_honored_only_from_trusted_disk_layers() {
    let layers = ConfigLayers {
        managed: toml::from_str(
                "[model_providers.gateway]\nbase_url = \"https://gateway.example/v1\"\n\
                 [model_providers.gateway.auth]\ncommand = \"/usr/local/bin/gw-token\"\n",
            )
            .unwrap(),
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert!(
            cfg.model_providers.contains_key("gateway"),
            "a model provider in a trusted disk layer is honored"
        );
    assert_eq!(
            cfg.auth_providers
                .get("model_provider:gateway")
                .map(|c| c.command.as_str()),
            Some("/usr/local/bin/gw-token"),
            "its inline auth registers as a synthetic auth provider"
        );
}
/// REGRESSION: the real enterprise two-file merge must resolve the deployment-config fetch to cli-chat-proxy, never the model host. It must also preserve the customer's S3 trace-upload endpoint.
/// The merge layers `managed_config.toml` (proxy and BYO model host) with `requirements.toml` (deployment key and S3 trace upload). It runs via the actual `ConfigLayers::effective_config()` path.
#[test]
#[serial_test::serial]
fn enterprise_two_file_merge_routes_deployment_key_to_proxy() {
    for k in [
        "GROK_MANAGED_CONFIG_URL",
        "GROK_CLI_CHAT_PROXY_BASE_URL",
        "GROK_TRACE_UPLOAD_ENDPOINT_URL",
    ] {
        unsafe { std::env::remove_var(k) };
    }
    let managed = toml::from_str(
            r#"
[endpoints]
xai_api_base_url = "https://inference.acme-corp.example/xai/v1"
cli_chat_proxy_base_url = "https://cli-chat-proxy.grok.com/v1"

[model.remote-code]
base_url = "https://inference.acme-corp.example/xai/v1"
env_key = "ANTHROPIC_AUTH_TOKEN"
model = "grok-4.5"

[models]
default = "grok-4.5"
"#,
        )
        .unwrap();
    let requirements = toml::from_str(
            r#"
[features]
feedback = true
telemetry = false

[endpoints]
deployment_key = "xai-token-ENTERPRISE"
xai_api_base_url = "https://inference.acme-corp.example/xai/v1"
trace_upload_bucket = "s3://acme-trace"
trace_upload_endpoint_url = "https://s3.acme-corp.example"
"#,
        )
        .unwrap();
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed,
        user: toml::Value::Table(Default::default()),
        user_requirements: Some(requirements),
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(
            cfg.endpoints.resolve_managed_config_url(),
            "https://cli-chat-proxy.grok.com/v1/deployment/config"
        );
    assert!(
            !cfg.endpoints
                .resolve_managed_config_url()
                .contains("acme-corp")
        );
    assert_eq!(
            cfg.endpoints.trace_upload_endpoint_url.as_deref(),
            Some("https://s3.acme-corp.example")
        );
    assert!(cfg.endpoints.deployment_key.is_some());
}
/// `[feedback.user]` in the managed layer must survive the layer merge into the resolved `Config` (its presence is the opt-in).
#[test]
fn managed_config_feedback_user_reaches_resolved_config() {
    let managed = toml::from_str(
            r#"
[endpoints]
cli_chat_proxy_base_url = "https://cli-chat-proxy.grok.com/v1"

[feedback.user]
name = ["os_user"]
email = ["git_email", "team@example.com"]
email_domain = "example.com"
"#,
        )
        .unwrap();
    let layers = ConfigLayers {
        managed,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    let user = cfg
        .feedback
        .user
        .expect("[feedback.user] from managed_config.toml must reach Config");
    assert_eq!(user.name, vec!["os_user"]);
    assert_eq!(user.email, vec!["git_email", "team@example.com"]);
    assert_eq!(user.email_domain.as_deref(), Some("example.com"));
    let layers = ConfigLayers::default();
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(cfg.feedback.user, None);
}
/// RCE guard: a project `.grok/config.toml` must never source `[feedback.user]` (its `command` runs `sh -c`).
#[test]
#[serial_test::serial]
fn project_config_never_sources_feedback_user() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let grok = repo.path().join(".grok");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::write(
            grok.join("config.toml"),
            "[plugins]\npaths = [\"./p\"]\n\n[feedback.user]\ncommand = \"/evil\"\n",
        )
        .unwrap();
    let cwd = repo.path();
    xai_grok_workspace::folder_trust::grant_folder_trust(cwd);
    assert!(
            resolve_effective_plugins_config(cwd)
                .paths
                .iter()
                .any(|p| p == "./p"),
            "trusted project [plugins].paths must merge (proves the project config is read)"
        );
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &load_effective_config().unwrap(),
        )
        .unwrap();
    assert_eq!(
            cfg.feedback.user, None,
            "a project [feedback.user] must never reach Config (would be sh -c RCE)"
        );
}
#[test]
fn config_layers_origins_tracks_source() {
    use crate::agent::config::ConfigSource;
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\ntelemetry = false\n").unwrap(),
        user: toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let origins = config_origins(&layers);
    assert_eq!(
            origins.get("features.telemetry").copied(),
            Some(ConfigSource::ManagedConfig)
        );
    assert_eq!(
            origins.get("ui.theme").copied(),
            Some(ConfigSource::UserConfig)
        );
}
#[test]
fn config_layers_origins_user_wins() {
    use crate::agent::config::ConfigSource;
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\ntelemetry = false\n").unwrap(),
        user: toml::from_str("[features]\ntelemetry = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let origins = config_origins(&layers);
    assert_eq!(
            origins.get("features.telemetry").copied(),
            Some(ConfigSource::UserConfig)
        );
}
#[test]
fn config_layers_system_managed_lowest_priority() {
    let layers = ConfigLayers {
        system_managed: toml::from_str("[features]\ntelemetry = false\n").unwrap(),
        managed: toml::Value::Table(Default::default()),
        user: toml::from_str("[features]\ntelemetry = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(
            Some(crate::agent::config::TelemetryMode::Enabled),
            cfg.features.telemetry
        );
}
#[test]
fn apply_requirements_value_overrides_user_settings() {
    let raw_config: toml::Value = toml::from_str(
            "[cli]\nauto_update = true\nchannel = \"beta\"\n\n[features]\ntelemetry = true\nfeedback = true\nlsp_tools = true\nweb_fetch = true\nwrite_file = true\n\n[telemetry]\ntrace_upload = true\n\n[ui]\nyolo = true\n\n[models]\ndefault = \"user-model\"\nweb_search = \"user-ws-model\"\n\n[endpoints]\ncli_chat_proxy_base_url = \"https://user-proxy.example/v1\"\nxai_api_base_url = \"https://user-api.example/v1\"\nmodels_base_url = \"https://user-models.example/v1\"\nmodels_list_url = \"https://user-models.example/v1/models\"\n",
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw_config).unwrap();
    cfg.default_yolo_mode = true;
    let requirements: toml::Value = toml::from_str(
            "[cli]\nauto_update = false\nchannel = \"stable\"\n\n[features]\ntelemetry = false\nfeedback = false\nlsp_tools = false\nweb_fetch = false\nwrite_file = false\nremote_fetch = false\n\n[telemetry]\ntrace_upload = false\nmixpanel_enabled = false\nmixpanel_token = \"enterprise-mp-token\"\n\n[ui]\nyolo = false\n\n[models]\ndefault = \"managed-model\"\nweb_search = \"managed-ws-model\"\n\n[endpoints]\ncli_chat_proxy_base_url = \"https://managed-proxy.example/v1\"\nxai_api_base_url = \"https://managed-api.example/v1\"\nmodels_base_url = \"https://managed-models.example/v1\"\nmodels_list_url = \"https://managed-models.example/v1/models\"\ndeployment_key = \"enterprise-deploy-key-should-not-log\"\ntrace_upload_endpoint_url = \"https://s3.custom.example.com\"\ntrace_upload_credentials = '{\"aws_access_key_id\":\"AKTEST\",\"aws_secret_access_key\":\"secret\"}'\n",
        )
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &requirements, &source);
    assert_eq!(
            Some(crate::agent::config::TelemetryMode::Disabled),
            cfg.features.telemetry
        );
    assert!(!cfg.is_feature_enabled(crate::agent::config::Feature::Feedback));
    assert!(!cfg.is_feature_enabled(crate::agent::config::Feature::LspTools));
    assert!(!cfg.is_feature_enabled(crate::agent::config::Feature::WebFetch));
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "features.lsp_tools" && e.value == "false")
        );
    assert!(!cfg.is_feature_enabled(crate::agent::config::Feature::WriteFile));
    assert_eq!(Some(false), cfg.requirements.remote_fetch.pinned());
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "features.remote_fetch" && e.value == "false")
        );
    assert_eq!(Some(false), cfg.telemetry.trace_upload);
    assert_eq!(Some(false), cfg.cli.auto_update);
    assert!(!cfg.ui.yolo);
    assert!(!cfg.default_yolo_mode);
    assert_eq!(Some("managed-model"), cfg.models.default.as_deref());
    assert_eq!(Some("managed-ws-model"), cfg.models.web_search.as_deref());
    assert_eq!(Some("stable"), cfg.cli.channel.as_deref());
    assert_eq!(
            Some("https://managed-proxy.example/v1"),
            cfg.endpoints.cli_chat_proxy_base_url.as_deref()
        );
    assert_eq!(
            "https://managed-api.example/v1",
            cfg.endpoints.xai_api_base_url
        );
    assert_eq!(
            Some("https://managed-models.example/v1"),
            cfg.endpoints.models_base_url.as_deref()
        );
    assert_eq!(
            Some("https://managed-models.example/v1/models"),
            cfg.endpoints.models_list_url.as_deref()
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "ui.yolo" && e.value == "--yolo blocked")
        );
    assert_eq!(
            Some("https://s3.custom.example.com"),
            cfg.endpoints.trace_upload_endpoint_url.as_deref()
        );
    assert!(
            cfg.endpoints.trace_upload_credentials.is_some(),
            "trace_upload_credentials should be set"
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "endpoints.trace_upload_credentials" && e.value == "[redacted]")
        );
    assert_eq!(
            Some("enterprise-deploy-key-should-not-log"),
            cfg.endpoints.deployment_key.as_deref()
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "endpoints.deployment_key" && e.value == "[redacted]"),
            "deployment_key must use the redacted enforce_str variant"
        );
    assert!(
            enforced
                .iter()
                .all(|e| e.path != "endpoints.deployment_key"
                    || e.value != "enterprise-deploy-key-should-not-log"),
            "raw deployment_key must not appear in enforced audit entries"
        );
    assert!(!cfg.telemetry.mixpanel_enabled);
    assert_eq!(
            Some("enterprise-mp-token"),
            cfg.telemetry.mixpanel_token.as_deref()
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "telemetry.mixpanel_token" && e.value == "[redacted]")
        );
}
/// Strict precedence: requirement always wins (covers from-None and from-higher-user cases).
/// The enforced floor lives in `VersionPolicy`, not this field.
#[test]
fn apply_requirements_pins_minimum_version() {
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let req: toml::Value = toml::from_str("[cli]\nminimum_version = \"0.1.150\"\n")
        .unwrap();
    let mut cfg_a = crate::agent::config::Config::new_from_toml_cfg(
            &toml::from_str::<toml::Value>("").unwrap(),
        )
        .unwrap();
    apply_requirements_inner(&mut cfg_a, &req, &source);
    assert_eq!(cfg_a.cli.minimum_version.as_deref(), Some("0.1.150"));
    let mut cfg_b = crate::agent::config::Config::new_from_toml_cfg(
            &toml::from_str::<toml::Value>("[cli]\nminimum_version = \"0.1.200\"\n")
                .unwrap(),
        )
        .unwrap();
    apply_requirements_inner(&mut cfg_b, &req, &source);
    assert_eq!(cfg_b.cli.minimum_version.as_deref(), Some("0.1.150"));
}
#[test]
fn apply_requirements_pins_voice_mode_false() {
    let mut cfg = crate::agent::config::Config::default();
    let req: toml::Value = toml::from_str("[features]\nvoice_mode = false\n").unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(
            cfg.requirements
                .pinned_feature(crate::agent::config::Feature::VoiceMode),
            Some(false)
        );
    assert!(!cfg.is_feature_enabled(crate::agent::config::Feature::VoiceMode));
}
/// Two layers pinning one key alike must report the layer that decided, not the first that asked.
/// Otherwise the operator log names a user's file for an administrator's pin.
/// Layers apply user first, system last.
#[test]
fn a_repeated_pin_is_reported_against_the_layer_that_decided() {
    let mut cfg = crate::agent::config::Config::default();
    let req: toml::Value = toml::from_str("[features]\nsession_search = false\n")
        .unwrap();
    let user = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/home/dev/.grok/requirements.toml"),
    };
    let system = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/etc/grok/requirements.toml"),
    };
    let mut enforced = apply_requirements_inner(&mut cfg, &req, &user);
    enforced.extend(apply_requirements_inner(&mut cfg, &req, &system));
    let deduped = keep_the_deciding_layer(enforced);
    let reported: Vec<_> = deduped
        .iter()
        .filter(|field| field.path == "features.session_search")
        .collect();
    assert_eq!(reported.len(), 1, "one row per pinned key");
    let Some(row) = reported.first() else {
        panic!("expected one reported pin: {reported:?}");
    };
    assert_eq!(row.source, system);
}
/// title_refresh is not a registry row, so the loop that pins those does not reach it.
/// An administrator pinning the title off must still outrank a user's GROK_TITLE_REFRESH, which a config-tier value would lose to.
#[test]
#[serial_test::serial]
fn apply_requirements_pins_title_refresh_over_the_environment() {
    use xai_grok_test_support::EnvGuard;
    let _env = EnvGuard::set("GROK_TITLE_REFRESH", "1");
    let mut cfg = crate::agent::config::Config::default();
    let req: toml::Value = toml::from_str("[features]\ntitle_refresh = false\n")
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &req, &source);
    let resolved = cfg.resolve_title_refresh();
    assert!(!resolved.value, "the pin lost to GROK_TITLE_REFRESH");
    assert_eq!(
            resolved.source,
            crate::agent::config::ConfigSource::Requirement
        );
    assert!(
            enforced
                .iter()
                .any(|field| field.path == "features.title_refresh"),
            "the pin is reported to the operator log"
        );
}
/// A non-boolean pin reaches the applier only when a higher layer supplied a valid value, so ignoring it leaves that winning layer standing.
#[test]
fn malformed_requirements_pin_is_ignored() {
    use crate::agent::config::Feature;
    let mut cfg = crate::agent::config::Config::default();
    let req: toml::Value = toml::from_str("[features]\nweb_fetch = 1\n").unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/home/dev/.grok/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(cfg.requirements.pinned_feature(Feature::WebFetch), None);
    assert!(
            !enforced
                .iter()
                .any(|e| e.path == Feature::WebFetch.path()),
            "an ignored pin enforces nothing"
        );
}
/// Requirements enforcement beats a campaign-supplied default.
/// The on-disk `Config` arrives campaign-overlaid (`models.default` holds a campaign value).
/// A requirements layer enforcing `[models] default` clamps it back.
#[test]
fn apply_requirements_default_beats_campaign_default() {
    let raw: toml::Value = toml::from_str("[models]\ndefault = \"campaign-model\"\n")
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    assert_eq!(
            cfg.models.default.as_deref(),
            Some("campaign-model"),
            "precondition: config carries the campaign default"
        );
    let req: toml::Value = toml::from_str("[models]\ndefault = \"enforced-model\"\n")
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(
            cfg.models.default.as_deref(),
            Some("enforced-model"),
            "requirements default must beat the campaign default"
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "models.default" && e.value == "enforced-model"),
            "the enforcement must be reported in the audit trail"
        );
}
fn pin_allowed_models(cfg: &mut crate::agent::config::Config, req_toml: &str) {
    let req: toml::Value = toml::from_str(req_toml).unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    apply_requirements_inner(cfg, &req, &source);
}
#[test]
fn apply_requirements_allowed_models_clamps_catalog_and_names_source() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            default = "grok-3"
            allowed_models = ["*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = [\"grok-4\"]\n");
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let selectable = |id| {
        catalog
            .get(id)
            .unwrap_or_else(|| panic!("expected {id}: {catalog:?}"))
            .info
            .user_selectable
    };
    assert!(
            selectable("grok-4"),
            "signed allowlist member must stay selectable"
        );
    assert!(
            !selectable("grok-3"),
            "models outside the signed set must not be selectable"
        );
    let err = crate::agent::remote_config::validate_selectable(&cfg, &catalog)
        .unwrap_err();
    assert!(
            err.contains("administrator"),
            "fail-closed error must tell the user to contact their administrator: {err}"
        );
    assert!(
            !err.contains("requirements.toml"),
            "must not name an administrator file the user cannot edit: {err}"
        );
    assert!(err.contains("grok-3"), "error must name the excluded default: {err}");
}
#[test]
fn apply_requirements_allowed_models_ignores_user_catalog_key() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            default = "grok-4"
            allowed_models = ["*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4-anything]
            model = "other-model"
            base_url = "https://evil.example/v1"
            context_window = 256000
            [model.my-alias]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = [\"grok-4*\"]\n");
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let selectable = |id| {
        catalog
            .get(id)
            .unwrap_or_else(|| panic!("expected {id}: {catalog:?}"))
            .info
            .user_selectable
    };
    assert!(
            selectable("grok-4"),
            "routing slug grok-4 matches grok-4*"
        );
    assert!(
            selectable("my-alias"),
            "user alias whose model id is grok-4 stays selectable"
        );
    assert!(
            !selectable("grok-4-anything"),
            "catalog key grok-4-anything pointing at another model must not satisfy the pin"
        );
}
#[test]
fn apply_requirements_malformed_allowed_models_fail_closes() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            default = "grok-4"
            allowed_models = ["*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = \"grok-4\"\n");
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let Some(grok4) = catalog.get("grok-4") else {
        panic!("expected grok-4: {catalog:?}");
    };
    assert!(
            !grok4.info.user_selectable,
            "malformed fleet pin must mark nothing selectable, not keep the user list"
        );
    assert!(
            matches!(
                cfg.requirements.allowed_models.pin_ref(),
                Some(crate::agent::config::AllowlistPin::FailClosed)
            ),
            "unreadable pin must be FailClosed, not a reserved glob"
        );
    let err = crate::agent::remote_config::validate_selectable(&cfg, &catalog)
        .unwrap_err();
    assert!(
            err.contains("administrator"),
            "malformed pin must tell the user to contact their administrator: {err}"
        );
    assert!(
            !err.contains("allowed_models"),
            "fleet invalid-policy message must not tell the user to edit allowed_models: {err}"
        );
}
#[test]
fn apply_requirements_allowed_models_empty_array_is_unrestricted() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            allowed_models = ["grok-4"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = []\n");
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let selectable = |id| {
        catalog
            .get(id)
            .unwrap_or_else(|| panic!("expected {id}: {catalog:?}"))
            .info
            .user_selectable
    };
    assert!(
            selectable("grok-3") && selectable("grok-4"),
            "empty fleet array must not restrict"
        );
    assert!(
            matches!(
                cfg.requirements.allowed_models.pin_ref(),
                Some(crate::agent::config::AllowlistPin::List(v)) if v.is_empty()
            ),
            "empty array is an explicit unrestricted pin"
        );
    assert_eq!(
            cfg.models.allowed_models,
            Some(vec!["grok-4".to_string()]),
            "pin must not overwrite the user-field copy; EffectiveAllowlist reads the pin"
        );
}
#[test]
fn apply_requirements_allowed_models_replaces_user_list() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            allowed_models = ["*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = [\"grok-4\"]\n");
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let selectable = |id| {
        catalog
            .get(id)
            .unwrap_or_else(|| panic!("expected {id}: {catalog:?}"))
            .info
            .user_selectable
    };
    assert!(selectable("grok-4"));
    assert!(
            !selectable("grok-3"),
            "user * must not union with the fleet pin"
        );
}
#[test]
fn validate_selectable_rejects_dash_m_outside_fleet_pin() {
    let raw: toml::Value = toml::from_str(
            r#"
            [models]
            default = "grok-4"
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    pin_allowed_models(&mut cfg, "[models]\nallowed_models = [\"grok-4\"]\n");
    cfg.default_model_override = Some("grok-3".into());
    let catalog = crate::agent::remote_config::resolve_model_catalog(&cfg, None);
    let err = crate::agent::remote_config::validate_selectable(&cfg, &catalog)
        .unwrap_err();
    assert!(err.contains("-m flag"), "must name the -m source: {err}");
    assert!(err.contains("administrator"), "fleet -m deny must be admin language: {err}");
    assert!(
            !err.contains("Broaden"),
            "must not tell the user to edit the fleet list: {err}"
        );
}
#[test]
fn apply_requirements_telemetry_string_form_pins_known_modes_only() {
    use crate::agent::config::TelemetryMode;
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let apply = |toml_str: &str| {
        let raw = toml::Value::Table(toml::map::Map::new());
        let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
        let req: toml::Value = toml::from_str(toml_str).unwrap();
        let enforced = apply_requirements_inner(&mut cfg, &req, &source);
        (cfg, enforced)
    };
    let (cfg, enforced) = apply("[features]\ntelemetry = \"session_metrics\"\n");
    assert_eq!(
            cfg.requirements.telemetry.pinned(),
            Some(TelemetryMode::SessionMetrics),
        );
    assert!(
            enforced
                .iter()
                .any(|e| e.path == "features.telemetry" && e.value == "session_metrics"),
        );
    let (cfg, enforced) = apply("[features]\ntelemetry = \"garbage\"\n");
    assert_eq!(cfg.requirements.telemetry.pinned(), None);
    assert!(!enforced.iter().any(|e| e.path == "features.telemetry"));
}
#[test]
fn validate_hooks_path_rejects_relative_path() {
    let result = validate_hooks_path("relative/path/hooks");
    assert!(result.is_err());
    assert!(
            result.unwrap_err().to_string().contains("absolute"),
            "should mention 'absolute'"
        );
}
#[test]
fn validate_hooks_path_rejects_outside_grok_home() {
    let result = validate_hooks_path("/tmp/evil-hooks");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
            msg.contains("must be under ~/.grok/"),
            "should mention ~/.grok/ restriction, got: {msg}"
        );
}
#[test]
fn validate_hooks_path_rejects_traversal_attack() {
    let grok_home = crate::util::grok_home::grok_home();
    let traversal = format!("{}/../evil", grok_home.display());
    let result = validate_hooks_path(&traversal);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
            msg.contains("must be under ~/.grok/"),
            "traversal should be rejected, got: {msg}"
        );
}
#[test]
fn validate_hooks_path_accepts_grok_hooks_subdir() {
    let grok_home = crate::util::grok_home::grok_home();
    let valid_path = grok_home.join("hooks").join("my-hooks");
    let _ = std::fs::create_dir_all(&valid_path);
    let result = validate_hooks_path(valid_path.to_str().unwrap());
    assert!(result.is_ok(), "path under ~/.grok/ should be accepted");
}
#[test]
fn managed_settings_disables_features_and_requirements_overrides() {
    use crate::agent::config::Feature;
    use xai_grok_workspace::permission::resolution::ManagedSettingsFeatures;
    let mut cfg = crate::agent::config::Config::default();
    cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
    cfg.feature_values.insert(Feature::Feedback, true);
    cfg.default_yolo_mode = true;
    let features = ManagedSettingsFeatures {
        disable_telemetry: Some(true),
        disable_feedback: Some(true),
        disable_yolo: Some(true),
        source_path: Some(std::path::PathBuf::from("/etc/managed-settings.json")),
    };
    let enforced = apply_managed_settings_features_inner(&mut cfg, &features);
    assert_eq!(
            cfg.features.telemetry,
            Some(crate::agent::config::TelemetryMode::Disabled)
        );
    assert_eq!(cfg.feature_values.get(&Feature::Feedback), Some(&false));
    assert!(cfg.default_yolo_mode);
    assert_eq!(enforced.len(), 2);
    assert!(!enforced.iter().any(|e| e.path == "ui.yolo"));
    let req: toml::Value = toml::from_str(
            "[features]\ntelemetry = true\nfeedback = true\n\n[ui]\nyolo = true\n",
        )
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(
            cfg.features.telemetry,
            Some(crate::agent::config::TelemetryMode::Enabled)
        );
    assert!(cfg.is_feature_enabled(Feature::Feedback));
    assert!(cfg.ui.yolo);
}
/// REGRESSION: external managed-settings.json is advisory, not authoritative.
/// disableBypassPermissionsMode (mapped to features.disable_yolo) must NOT clamp the user's own grok yolo.
#[test]
fn managed_settings_does_not_override_user_yolo() {
    use crate::agent::config::Feature;
    use xai_grok_workspace::permission::resolution::ManagedSettingsFeatures;
    let mut cfg = crate::agent::config::Config::default();
    cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
    cfg.feature_values.insert(Feature::Feedback, true);
    cfg.ui.yolo = true;
    cfg.default_yolo_mode = true;
    let features = ManagedSettingsFeatures {
        disable_telemetry: Some(true),
        disable_feedback: Some(true),
        disable_yolo: Some(true),
        source_path: Some(
            std::path::PathBuf::from("/etc/claude-code/managed-settings.json"),
        ),
    };
    let enforced = apply_managed_settings_features_inner(&mut cfg, &features);
    assert_eq!(
            cfg.features.telemetry,
            Some(crate::agent::config::TelemetryMode::Disabled)
        );
    assert_eq!(cfg.feature_values.get(&Feature::Feedback), Some(&false));
    assert!(cfg.ui.yolo);
    assert!(cfg.default_yolo_mode);
    assert_eq!(enforced.len(), 2);
    assert!(!enforced.iter().any(|e| e.path == "ui.yolo"));
}
/// Simulate a release-stamped build so the folder-trust gate engages (a local/dev build auto-trusts).
/// Hold the returned guard for the test body.
fn simulate_release_build() -> xai_grok_test_support::EnvGuard {
    xai_grok_test_support::EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0.0-sim")
}
#[test]
fn project_overlay_tracks_authoritative_trust_transitions() {
    let source_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    write_subagent_definitions(
        &repo.path().join(".grok"),
        &[("shared", "Project"), ("project-only", "Project")],
    );
    let mut base = SubagentsConfig::default();
    base.roles
        .insert(
            "shared".into(),
            SubagentRole {
                description: "User role".into(),
                source_dir: Some(source_root.path().join("roles")),
                ..Default::default()
            },
        );
    base.personas
        .insert(
            "shared".into(),
            SubagentPersona {
                instructions: Some("User persona".into()),
                source_path: Some(
                    source_root.path().join("personas/shared.toml").display().to_string(),
                ),
                ..Default::default()
            },
        );
    let (untrusted_roles, _) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        repo.path(),
        false,
    );
    let Some(shared) = untrusted_roles.get("shared") else {
        panic!("expected shared role: {untrusted_roles:?}");
    };
    assert_eq!(shared.description, "User role");
    assert!(!untrusted_roles.contains_key("project-only"));
    let (trusted_roles, trusted_personas) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        repo.path(),
        true,
    );
    let Some(shared) = trusted_roles.get("shared") else {
        panic!("expected shared role: {trusted_roles:?}");
    };
    assert_eq!(shared.description, "Project role");
    assert!(trusted_personas.contains_key("project-only"));
    let (revoked_roles, _) = SubagentsConfig::effective_definition_maps(
        &base.roles,
        &base.personas,
        repo.path(),
        false,
    );
    let Some(shared) = revoked_roles.get("shared") else {
        panic!("expected shared role: {revoked_roles:?}");
    };
    assert_eq!(shared.description, "User role");
    assert!(!revoked_roles.contains_key("project-only"));
}
#[test]
fn base_resolver_without_project_cwd_keeps_project_files_out() {
    let tmp = tempfile::tempdir().unwrap();
    write_subagent_definitions(&tmp.path().join(".grok"), &[("project", "Project")]);
    let base = SubagentsConfig::resolve_base_with_sources(
        false,
        &toml::Value::Table(Default::default()),
        None,
        &tmp.path().join("bundled"),
    );
    assert!(base.get_role("project").is_none());
    assert!(base.get_persona("project").is_none());
}
#[test]
fn explicit_grok_root_is_the_only_user_source() {
    let tmp = tempfile::tempdir().unwrap();
    let ambient = tmp.path().join("ambient-home/.grok");
    let configured = tmp.path().join("configured-grok-home");
    write_subagent_definitions(&ambient, &[("ambient", "Ambient")]);
    write_subagent_definitions(&configured, &[("configured", "Configured")]);
    let base = SubagentsConfig::resolve_base_with_sources(
        false,
        &toml::Value::Table(Default::default()),
        Some(&configured),
        &configured.join("bundled"),
    );
    assert!(base.get_role("ambient").is_none());
    assert!(base.get_persona("ambient").is_none());
    assert!(base.get_role("configured").is_some());
    assert!(base.get_persona("configured").is_some());
}
/// SECURITY (plugin-RCE): a PROJECT-declared `[plugins].paths` loads as an auto-enabled, auto-trusted ConfigPath plugin.
/// It must therefore merge into the effective config ONLY when the folder is trusted; project `[plugins].disabled` is never gated.
/// The closing set-difference proves the gate toggles ONLY that path (user/global paths pass through both verdicts untouched). The test is GROK_HOME-isolated and `#[serial]` for folder-trust store hygiene: an empty store is deterministically untrusted. `EnvGuard` restores GROK_HOME even on panic. It is reliable only under nextest's process-per-test isolation.
#[test]
#[serial_test::serial]
fn resolve_effective_plugins_config_gates_project_paths_on_folder_trust() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let grok = repo.path().join(".grok");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::write(
            grok.join("config.toml"),
            "[plugins]\npaths = [\"./proj-plugin\"]\ndisabled = [\"proj-bad\"]\n",
        )
        .unwrap();
    let cwd = repo.path();
    let proj_path = "./proj-plugin".to_string();
    let proj_disabled = "proj-bad".to_string();
    let untrusted = resolve_effective_plugins_config(cwd);
    assert!(
            !untrusted.paths.contains(&proj_path),
            "untrusted folder must NOT merge the project [plugins].paths"
        );
    assert!(
            untrusted.disabled.contains(&proj_disabled),
            "project [plugins].disabled must merge even when untrusted (fail-safe)"
        );
    xai_grok_workspace::folder_trust::grant_folder_trust(cwd);
    let trusted = resolve_effective_plugins_config(cwd);
    assert!(
            trusted.paths.contains(&proj_path),
            "trusted folder must merge the project [plugins].paths"
        );
    assert!(
            trusted.disabled.contains(&proj_disabled),
            "project [plugins].disabled must merge when trusted too"
        );
    let trusted_minus_project: Vec<String> = trusted
        .paths
        .iter()
        .filter(|p| *p != &proj_path)
        .cloned()
        .collect();
    assert_eq!(
            trusted_minus_project, untrusted.paths,
            "the trust gate must toggle ONLY the project path; user/global paths unaffected"
        );
}
/// SECURITY (plugin-RCE) end-to-end, proved through the REAL `discover_plugins`. A PROJECT-declared `[plugins].paths` ConfigPath plugin is EXCLUDED from discovery while untrusted and included once trusted.
/// The Part-2 set-difference test covers the config merge. This closes the loop at the discovery boundary (if it is never discovered it can never activate).
/// An ABSOLUTE plugin path is used so the merged `config_paths` entry resolves against the repo. `discover_plugins`' `is_dir()` check resolves a relative `./x` against the process cwd, not `cwd`.
#[test]
#[serial_test::serial]
fn discover_plugins_excludes_untrusted_configpath_plugin_end_to_end() {
    use xai_grok_agent::plugins::{TrustStore, discover_plugins};
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let cwd = repo.path();
    let plugin_dir = cwd.join("cfgpath-probe");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("plugin.json"), r#"{"name":"cfgpath-probe"}"#)
        .unwrap();
    let grok = cwd.join(".grok");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::write(
            grok.join("config.toml"),
            format!("[plugins]\npaths = ['{}']\n", plugin_dir.display()),
        )
        .unwrap();
    let trust_store = TrustStore::load_from(home.path().join("plugin-trust"));
    let untrusted_dc = resolve_effective_plugins_config(cwd).to_discovery_config();
    let untrusted_verdict = crate::agent::folder_trust::project_scope_allowed(cwd);
    assert!(
            !untrusted_verdict,
            "a fresh repo declaring [plugins].paths must resolve untrusted"
        );
    assert!(
            !untrusted_dc
                .config_paths
                .iter()
                .any(|p| p.ends_with("cfgpath-probe")),
            "untrusted: the project path must be absent from config_paths"
        );
    let untrusted_found = discover_plugins(
            Some(cwd),
            &untrusted_dc,
            &trust_store,
            untrusted_verdict,
        )
        .iter()
        .any(|p| p.manifest.name == "cfgpath-probe");
    assert!(
            !untrusted_found,
            "untrusted folder must EXCLUDE the ConfigPath plugin from discovery"
        );
    xai_grok_workspace::folder_trust::grant_folder_trust(cwd);
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let trusted_dc = resolve_effective_plugins_config(cwd).to_discovery_config();
    let trusted_verdict = crate::agent::folder_trust::project_scope_allowed(cwd);
    assert!(trusted_verdict, "a store-granted repo must resolve trusted");
    let trusted_found = discover_plugins(
            Some(cwd),
            &trusted_dc,
            &trust_store,
            trusted_verdict,
        )
        .iter()
        .any(|p| p.manifest.name == "cfgpath-probe");
    assert!(
            trusted_found,
            "trusted folder must DISCOVER the merged ConfigPath plugin"
        );
}
/// Kill-switch ordering regression: `resolve_effective_plugins_config` reads the folder-trust gate internally. Its call sites (commands/list, plugin fan-out, reload) therefore resolve with the REAL RemoteSettings first.
/// A cold key under an org kill-switch must end up allowed. If the plugins-config read ran first, the gate's remote-less backstop would record a durable kill-switch-blind deny.
/// The `Some(false)` arm of `resolve_and_record_inner` (store-only reconcile) could never lift that deny. The test is GROK_HOME-isolated (empty store); GROK_FOLDER_TRUST is unset so the kill-switch is the only signal.
#[test]
#[serial_test::serial]
fn kill_switched_cold_cwd_stays_allowed_through_plugins_config_read() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let grok = repo.path().join(".grok");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::write(grok.join("config.toml"), "[plugins]\npaths = [\"./proj-plugin\"]\n")
        .unwrap();
    let cwd = repo.path();
    let remote = crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(false),
        ..Default::default()
    };
    assert!(
            crate::agent::folder_trust::resolve_and_record(cwd, Some(&remote), false),
            "kill-switch must resolve the cold key trusted"
        );
    let cfg = resolve_effective_plugins_config(cwd);
    assert!(
            cfg.paths.contains(&"./proj-plugin".to_string()),
            "kill-switched folder counts trusted, so the project path must merge"
        );
    assert!(
            crate::agent::folder_trust::project_scope_allowed(cwd),
            "gate must still allow the kill-switched folder after the config read"
        );
}
/// Writeback requires grok.com auth: remote may advertise it, but a non-xai credential is downgraded to `Local`.
#[test]
#[serial_test::serial]
fn from_remote_gated_requires_xai_auth_for_writeback() {
    let _env = crate::env::EnvVarGuard::remove("GROK_STORAGE_MODE");
    let writeback = crate::util::config::RemoteSettings {
        writeback_enabled: Some(true),
        ..Default::default()
    };
    assert_eq!(
        StorageMode::from_remote_gated(Some(&writeback), true),
        StorageMode::Writeback
    );
    assert_eq!(
        StorageMode::from_remote_gated(Some(&writeback), false),
        StorageMode::Local,
    );
    assert_eq!(
        StorageMode::from_remote_gated(None, true),
        StorageMode::Local
    );
}
#[test]
#[cfg(target_os = "linux")]
fn required_bwrap_routes_fail_closed() {
    assert_eq!(
        route_bwrap_startup(Some(()), false, true),
        BwrapStartup::ReexecRequired(())
    );
    assert_eq!(
        route_bwrap_startup::<()>(None, false, true),
        BwrapStartup::Refuse
    );
    assert_eq!(
        route_bwrap_startup::<()>(None, true, true),
        BwrapStartup::Verify
    );
}
#[test]
#[cfg(target_os = "linux")]
fn optional_bwrap_routes_can_degrade() {
    assert_eq!(
        route_bwrap_startup(Some(()), false, false),
        BwrapStartup::ReexecOptional(())
    );
    assert_eq!(
        route_bwrap_startup::<()>(None, false, false),
        BwrapStartup::Continue
    );
}
/// Concurrent `[plugins]` writers must not lose updates: each whole read-modify-write runs under the config-init flock.
#[test]
fn concurrent_plugin_list_writers_lose_no_updates() {
    let home = tempfile::tempdir().unwrap();
    let n = 8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let home = home.path().to_path_buf();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                update_config_toml_locked(
                        &home,
                        |table| plugins_list_add(
                            table,
                            "enabled",
                            &format!("plugin-{i}"),
                        ),
                    )
                    .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let content = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    let config: toml::Value = toml::from_str(&content).unwrap();
    let enabled = config
        .get("plugins")
        .and_then(|p| p.get("enabled"))
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(enabled.len(), n, "a concurrent enable was lost:\n{content}");
}
/// Bind dest before load. A retarget inside `mutate` must refuse — not merge
/// A's `[plugins]` onto B.
#[cfg(unix)]
#[test]
fn update_config_toml_locked_refuses_retarget_between_read_and_write() {
    let home = tempfile::tempdir().unwrap();
    let a = home.path().join("a.toml");
    let b = home.path().join("b.toml");
    std::fs::write(&a, "[ui]\nfrom_a = true\n").unwrap();
    std::fs::write(&b, "[ui]\nfrom_b = true\n").unwrap();
    let link = home.path().join("config.toml");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let err = update_config_toml_locked(
            home.path(),
            |table| {
                plugins_list_add(table, "enabled", "demo-plugin")?;
                std::fs::remove_file(&link).unwrap();
                std::os::unix::fs::symlink(&b, &link).unwrap();
                Ok(true)
            },
        )
        .expect_err("retarget between load and save must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("changed") || msg.contains("follow destination"),
        "expected retarget refusal, got {msg}"
    );
    let raw_a = std::fs::read_to_string(&a).unwrap();
    let raw_b = std::fs::read_to_string(&b).unwrap();
    assert!(
        raw_a.contains("from_a = true") && !raw_a.contains("demo-plugin"),
        "referent A must stay unmerged:\n{raw_a}"
    );
    assert!(
        raw_b.contains("from_b = true") && !raw_b.contains("demo-plugin"),
        "must not merge A's snapshot onto B:\n{raw_b}"
    );
    assert_eq!(b, std::fs::read_link(&link).unwrap());
}
/// Pins the blocking-pool hop behind the session `[plugins]` writers: LocalSet tasks keep running during a flock poll.
#[test]
fn plugin_config_writes_keep_the_caller_local_set_live() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;
    let home = tempfile::tempdir().unwrap();
    let held = crate::util::config::acquire_init_lock(home.path()).unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let local = tokio::task::LocalSet::new();
    let ticks = Rc::new(Cell::new(0u32));
    let ticker_ticks = Rc::clone(&ticks);
    rt.block_on(
        local
            .run_until(async move {
                tokio::task::spawn_local(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        ticker_ticks.set(ticker_ticks.get() + 1);
                    }
                });
                let home_path = home.path().to_path_buf();
                config_write_blocking(move || update_config_toml_locked(
                        &home_path,
                        |table| plugins_list_add(table, "enabled", "demo-plugin"),
                    ))
                    .await
                    .expect("write succeeds after the holder releases");
                assert!(
            ticks.get() >= 10,
            "LocalSet starved during a plugin config write: {} ticks",
            ticks.get()
        );
            }),
    );
    holder.join().unwrap();
}
