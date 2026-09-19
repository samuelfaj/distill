use super::{build_minimal_agent_for_tests, make_test_handle};
use agent_client_protocol as acp;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_permission_handle() {
    use xai_grok_workspace::permission::types::{
        PatternMode, PermissionConfig, PermissionRule, RuleAction, ToolFilter,
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_minimal_agent_for_tests();
            let sid = acp::SessionId::new("parent-permission");
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let gateway = GatewaySender::new(tx);
            let cwd = xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp"))
                .expect("absolute cwd");
            let (permission_handle, _events_rx) = xai_grok_workspace::permission::spawn_permission_manager(
                sid.clone(),
                gateway,
                cwd,
                xai_grok_workspace::permission::types::ClientType::Generic,
                Some(
                    PermissionConfig::new(
                        vec![PermissionRule {
                        action: RuleAction::Deny,
                        tool: ToolFilter::Read,
                        pattern: Some("**/.env".to_owned()),
                        pattern_mode: PatternMode::Glob,
                    }],
                    ),
                ),
                Vec::new(),
                Vec::new(),
                false,
                None,
            );
            let mut handle = make_test_handle("test-model", false, None);
            handle.permission_handle = permission_handle;
            agent.insert_resident(&sid, handle);
            let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
            let inherited = ctx
                .permission_handle
                .expect("subagent context must inherit parent permission handle");
            for access in [
                xai_grok_workspace::permission::AccessKind::Read(Some(".env".into())),
                xai_grok_workspace::permission::AccessKind::Bash("cat .env".into()),
            ] {
                let decision = inherited
                    .request(xai_grok_workspace::permission::PermissionRequest {
                        session_id: Some("child-session".to_owned()),
                        subagent_type: Some("general-purpose".to_owned()),
                        subagent_description: Some(
                            "permission inheritance regression".to_owned(),
                        ),
                        ..xai_grok_workspace::permission::PermissionRequest::new(
                            access.clone(),
                            acp::ToolCallUpdate::new(
                                acp::ToolCallId::new("tc"),
                                Default::default(),
                            ),
                        )
                    })
                    .await
                    .decision;
                assert!(
                    matches!(
                        decision,
                        xai_grok_workspace::permission::Decision::PolicyDeny(_)
                    ),
                    "subagent-inherited handle must enforce parent deny for {access:?}, got {decision:?}"
                );
            }
        })
        .await;
}
#[tokio::test]
async fn subagent_spawn_context_shares_active_message_parent_prompt_index() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-telemetry");
    let handle = make_test_handle("test-model", false, None);
    let live_prompt_index = handle
        .tool_context
        .active_message_parent_prompt_index
        .clone();
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    live_prompt_index.store(6, std::sync::atomic::Ordering::Release);
    assert_eq!(
        ctx.active_message_parent_prompt_index
            .load(std::sync::atomic::Ordering::Acquire),
        6,
    );
}
#[tokio::test]
#[serial_test::serial(remote_sig_disarm)]
async fn memory_reapply_updates_future_spawn_contexts_only() {
    use crate::config::{MemoryConfig, MemoryMode};
    let mut agent = build_minimal_agent_for_tests();
    let initial_memory = MemoryConfig {
        root_dir_override: Some(std::path::PathBuf::from("/tmp/test-memory-root")),
        ..Default::default()
    };
    agent.set_memory_config(initial_memory);
    let sid = acp::SessionId::new("parent-memory-refresh");
    let mut parent = make_test_handle("test-model", false, None);
    parent.spawn_snapshot.memory_mode = Some(MemoryMode::Legacy);
    agent.insert_resident(&sid, parent);
    let existing_spawn = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        existing_spawn
            .memory_config
            .as_ref()
            .map(|config| (config.enabled, config.mode)),
        Some((false, MemoryMode::Legacy)),
        "the initial setter must seed spawn snapshots"
    );
    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
        memory_v2: Some(crate::config::MemoryV2Settings {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    });
    agent.on_remote_settings_changed();
    let future_spawn = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        future_spawn
            .memory_config
            .as_ref()
            .map(|config| (config.enabled, config.mode)),
        Some((true, MemoryMode::V2)),
        "a remote memory-v2 gate must reach contexts built after settings apply"
    );
    assert_eq!(
        future_spawn
            .memory_config
            .as_ref()
            .and_then(|config| config.root_dir_override.as_deref()),
        Some(std::path::Path::new("/tmp/test-memory-root")),
        "runtime-only memory paths must survive remote re-resolution"
    );
    assert_eq!(
        existing_spawn
            .memory_config
            .as_ref()
            .map(|config| (config.enabled, config.mode)),
        Some((false, MemoryMode::Legacy)),
        "an already-built spawn context must keep its session-pinned memory config"
    );
    assert_eq!(
        agent
            .resident_handle(&sid)
            .unwrap()
            .spawn_snapshot
            .memory_mode,
        Some(MemoryMode::Legacy),
        "the existing session snapshot must remain unchanged"
    );
}
#[tokio::test]
async fn subagent_spawn_context_shares_parent_goal_loop_gate() {
    use std::sync::atomic::Ordering::Relaxed;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-goal");
    let handle = make_test_handle("test-model", false, None);
    let parent_gate = handle.tool_context.goal_loop_active_gate.clone();
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert!(!ctx.goal_loop_active.load(Relaxed));
    parent_gate.store(true, Relaxed);
    assert!(
        ctx.goal_loop_active.load(Relaxed),
        "subagent context must observe the parent's goal-loop gate (same Arc)"
    );
}
#[tokio::test]
async fn subagent_spawn_context_disables_ask_user_question_from_enabled_parent() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-ask-enabled");
    let mut handle = make_test_handle("test-model", false, None);
    handle.ask_user_question_enabled = true;
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert!(
        !ctx.ask_user_question_enabled,
        "subagent must not inherit the enabled parent ask_user_question gate"
    );
}
#[tokio::test]
async fn subagent_spawn_context_carries_parent_paths_config() {
    let agent = build_minimal_agent_for_tests();
    let paths = crate::agent::config::PathsConfig {
        extra_rule_dirs: vec![
            "~/team-rules".to_owned(),
            "/opt/company/grok-rules".to_owned(),
        ],
        ..Default::default()
    };
    agent.cfg.borrow_mut().paths = paths.clone();
    let sid = acp::SessionId::new("parent-paths");
    agent.insert_resident(&sid, make_test_handle("test-model", false, None));
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        paths, ctx.parent_paths_config,
        "child rules discovery must see the parent's `[paths]` table"
    );
}
#[tokio::test]
async fn subagent_spawn_context_carries_parent_bash_tool_params() {
    let agent = build_minimal_agent_for_tests();
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.toolset.bash.max_timeout_secs = Some(36_000.0);
        cfg.toolset.bash.output_byte_limit = Some(65_536);
        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
            auto_background_on_timeout: Some(false),
            ..Default::default()
        });
    }
    let sid = acp::SessionId::new("parent-bash-params");
    agent.insert_resident(&sid, make_test_handle("test-model", false, None));
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    let expected_bash = serde_json::json!({
        "max_timeout_secs": 36_000.0,
        "output_byte_limit": 65_536,
        "auto_background_on_timeout": false,
        "allow_background_operator": true,
    });
    assert_eq!(
        ctx.tool_params_json.bash,
        expected_bash.as_object().cloned(),
        "child bash params must equal the parent's resolved [toolset.bash] (incl. remote fallbacks)"
    );
    let spawn_src = include_str!("../../subagent/handle_request.rs");
    assert!(
        spawn_src.contains("std::mem::take(&mut ctx.tool_params_json)"),
        "run_shell_child must forward ctx.tool_params_json into spawn_session_on_thread"
    );
}
#[tokio::test]
async fn subagent_spawn_context_copies_parent_non_interactive() {
    let agent = build_minimal_agent_for_tests();
    let sid_headless = acp::SessionId::new("parent-headless");
    let mut handle_headless = make_test_handle("test-model", false, None);
    handle_headless.non_interactive = true;
    agent.insert_resident(&sid_headless, handle_headless);
    let ctx_headless = agent.build_subagent_spawn_context(sid_headless.0.as_ref());
    assert!(
        ctx_headless.parent_non_interactive,
        "subagent must copy the parent's non_interactive flag (headless -p parent)"
    );
    let sid_tui = acp::SessionId::new("parent-tui");
    agent.insert_resident(&sid_tui, make_test_handle("test-model", false, None));
    let ctx_tui = agent.build_subagent_spawn_context(sid_tui.0.as_ref());
    assert!(
        !ctx_tui.parent_non_interactive,
        "an interactive parent must not mark its subagents non-interactive"
    );
}
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_configured_cutoff() {
    let agent = build_minimal_agent_for_tests();
    let cutoff = xai_grok_sampling_types::ToolOverrides {
        x_search: Some(xai_grok_sampling_types::XSearchOptions {
            date_bound: Some(
                xai_grok_sampling_types::SearchDateBound::new(None, Some("2020-01-01".to_string()))
                    .unwrap(),
            ),
        }),
        web_search: None,
    };
    let sid = acp::SessionId::new("parent-cutoff");
    let handle = make_test_handle("test-model", false, None);
    handle
        .resolved_tool_overrides
        .store(Some(std::sync::Arc::new(cutoff.clone())));
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        ctx.inherited_tool_overrides,
        Some(cutoff),
        "subagent context must inherit the parent's configured cutoff for its first-turn update"
    );
    let sid_none = acp::SessionId::new("parent-unbounded");
    agent.insert_resident(&sid_none, make_test_handle("test-model", false, None));
    let ctx_none = agent.build_subagent_spawn_context(sid_none.0.as_ref());
    assert!(
        ctx_none.inherited_tool_overrides.is_none(),
        "an unbounded parent must not hand a subagent a cutoff"
    );
}
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_process_scope() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-process-scope");
    let mut handle = make_test_handle("test-model", false, None);
    let parent_scope = xai_tty_utils::ProcessScope::new();
    handle.tool_context.process_scope = Some(parent_scope.clone());
    agent.insert_resident(&sid, handle);
    let owner = std::sync::Arc::new(xai_tty_utils::ProcessGroup::new().expect("process group"));
    parent_scope.register(&owner);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    let inherited = ctx
        .process_scope
        .expect("subagent context must inherit the parent's process scope");
    assert_eq!(
        inherited.live_count(),
        1,
        "the child sees the owner enrolled through the parent scope"
    );
}
fn model_entry_with_rate_limit(
    slug: &str,
    attempts: Option<u32>,
) -> crate::agent::config::ModelEntry {
    let mut info = crate::agent::config::ModelInfo::fallback(slug);
    info.subagent_rate_limit_max_attempts = attempts;
    crate::agent::config::ModelEntry {
        info,
        mtls_cert_dir: None,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}
#[tokio::test]
async fn subagent_spawn_context_resolves_rate_limit_attempts_against_child_model() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-rate-limit");
    agent.insert_resident(&sid, make_test_handle("parent-model", false, None));
    let mut ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    let mut models = indexmap::IndexMap::new();
    models.insert(
        "parent-model".to_string(),
        model_entry_with_rate_limit("parent-model", Some(4)),
    );
    models.insert(
        "child-model".to_string(),
        model_entry_with_rate_limit("child-model", Some(0)),
    );
    ctx.available_models = models;
    assert_eq!(
        ctx.resolve_subagent_rate_limit_max_attempts("child-model"),
        0,
        "a subagent on a different model must honor that model's disable (0), not the parent's"
    );
    assert_eq!(
        ctx.resolve_subagent_rate_limit_max_attempts("parent-model"),
        4,
        "the per-model lookup keys on the passed model id"
    );
}
#[test]
#[serial_test::serial]
fn subagent_spawn_context_resolves_compaction_mode_like_parent() {
    use crate::agent::config::Config;
    use xai_chat_state::{CompactionDetail, CompactionMode};
    use xai_grok_test_support::EnvGuard;
    let _mode = EnvGuard::unset("GROK_COMPACTION_MODE");
    let _detail = EnvGuard::unset("GROK_COMPACTION_DETAIL");
    let mut ctx = crate::test_support::lsp_runtime::ctx_with_toggle(Default::default());
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::default(),
        "empty spawn context must inherit the compiled Segments default, not Summary"
    );
    let mut summary_cfg = Config::default();
    summary_cfg.features.compaction_mode = Some("summary".into());
    ctx.agent_config = Some(summary_cfg);
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Summary,
        "parent [features].compaction_mode must win over remote/default"
    );
    ctx.agent_config = None;
    ctx.remote_settings = Some(crate::util::config::RemoteSettings {
        compaction_mode: Some("segments".into()),
        compaction_detail: Some("minimal".into()),
        ..Default::default()
    });
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Segments(CompactionDetail::Minimal),
        "remote mode+detail must attach via with_segment_detail"
    );
    let mut segments_cfg = Config::default();
    segments_cfg.features.compaction_mode = Some("segments".into());
    segments_cfg.features.compaction_detail = Some("balanced".into());
    ctx.agent_config = Some(segments_cfg);
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Segments(CompactionDetail::Balanced),
        "parent config detail must win over remote detail"
    );
    let _env_mode = EnvGuard::set("GROK_COMPACTION_MODE", "transcript");
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Transcript,
        "GROK_COMPACTION_MODE must win over parent config and remote"
    );
}
#[test]
fn run_shell_child_passes_parent_compaction_pins_into_spawn() {
    use crate::agent::subagent::SubagentSpawnContext;
    use crate::session::CompactionPins;
    use xai_chat_state::CompactionMode;
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let default_child = UserMessageTemplate::Default;
    let mut ctx = crate::test_support::lsp_runtime::ctx_with_toggle(Default::default());
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::default(),
        two_pass: true,
    };
    assert_eq!(
        ctx.compaction_pins_for_child(&default_child),
        CompactionPins {
            mode: CompactionMode::default(),
            two_pass: true,
        },
    );
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::Summary,
        two_pass: false,
    };
    assert_eq!(
        ctx.compaction_pins_for_child(&default_child),
        CompactionPins {
            mode: CompactionMode::Summary,
            two_pass: false,
        },
    );
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::default(),
        two_pass: true,
    };
    assert_eq!(
        SubagentSpawnContext::snapshot_parent_compaction_pins(
            CompactionMode::default(),
            true,
            Some("remote-code"),
            Some("remote-code"),
            std::path::Path::new("/tmp"),
        ),
        CompactionPins {
            mode: CompactionMode::default(),
            two_pass: true,
        },
    );
    let spawn_src = include_str!("../../subagent/handle_request.rs");
    assert!(
        spawn_src.contains("ctx.compaction_pins_for_child(&definition.user_message_template)"),
        "run_shell_child must pass compaction_pins_for_child into spawn"
    );
    assert!(
        spawn_src.contains("pins.two_pass"),
        "run_shell_child must pass pins.two_pass into spawn_session_on_thread"
    );
    assert!(
        !spawn_src.contains("CompactionMode::Summary"),
        "run_shell_child must not hard-pin Summary at the spawn site"
    );
    assert!(
        !spawn_src.contains("two_pass_enabled = false")
            && !spawn_src.contains("false, // two_pass"),
        "run_shell_child must not hard-pin two-pass off at the spawn site"
    );
}
