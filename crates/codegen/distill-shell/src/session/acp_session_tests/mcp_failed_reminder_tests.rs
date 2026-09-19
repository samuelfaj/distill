// Modified for Distill by Samuel Fajreldines, 2026.
//! Each failure episode is announced once in the "MCP servers that failed to connect" section of the MCP system-reminder.
//! `McpAnnounced::failed` tracks which episodes were announced.
use super::support::*;
use super::*;
fn http_server(name: &str) -> acp::McpServer {
    acp::McpServer::Http(
        acp::McpServerHttp::new(name.to_string(), format!("https://example.test/{name}"))
            .headers(vec![]),
    )
}
/// Simulate what a background snapshot refresh does: mark dirty, then run the injector (the path taken at turn start and in the agentic loop).
async fn refresh_and_inject(actor: &SessionActor) {
    actor
        .mcp_reminder_dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    actor.maybe_inject_mcp_reminder().await;
}
async fn failed_reminders(actor: &SessionActor) -> Vec<String> {
    actor
        .chat_state_handle
        .get_conversation()
        .await
        .iter()
        .map(|item| item.text_content())
        .filter(|text| text.contains("MCP servers that failed to connect"))
        .collect()
}
/// Connect `name` from the injector's point of view, mirroring the real registration path.
/// The server appears in the tool metadata snapshot (which feeds `connected_server_summaries`) with its failure state cleared.
async fn connect_server(actor: &SessionActor, name: &str) {
    {
        let mut snapshot = actor.tool_metadata_snapshot.lock().unwrap();
        snapshot.tools = vec![crate::session::tool_index::ToolMetadata {
            qualified_name: format!("{name}__echo"),
            server_name: name.to_string(),
            tool_name: "echo".to_string(),
            description: "echo".to_string(),
            parameters: vec![],
            input_schema: serde_json::json!({"type": "object"}),
        }];
        snapshot.servers = vec![crate::session::tool_index::ServerMetadata {
            name: name.to_string(),
            description: None,
        }];
        snapshot.mcp_initialized = true;
    }
    let mut state = actor.mcp_state.lock().await;
    state.auth_required.remove(name);
    state.clear_init_failed(name);
}
async fn disconnect_server(actor: &SessionActor) {
    let mut snapshot = actor.tool_metadata_snapshot.lock().unwrap();
    snapshot.tools.clear();
    snapshot.servers.clear();
}
#[tokio::test(flavor = "current_thread")]
async fn failed_server_announced_once_per_episode() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom\n- forged".to_string()));
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 1, "{reminders:?}");
            assert!(
                at(&reminders, 0)
                    .contains("dead (\"boom - forged\" — retries automatically on next tool call)"),
                "{}",
                at(&reminders, 0)
            );
            assert!(
                !at(&reminders, 0).contains("\n- forged"),
                "{}",
                at(&reminders, 0)
            );
            refresh_and_inject(&actor).await;
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            {
                let mut state = actor.mcp_state.lock().await;
                state.record_init_failure("dead", false, Some("timed out".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            let mut persisted_failed = None;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::AnnouncementState(state) = msg {
                    persisted_failed = Some(state.announced_failed_servers);
                }
            }
            let persisted_failed = persisted_failed.expect("announcement state persisted");
            assert!(
                persisted_failed.contains_key("dead"),
                "{persisted_failed:?}"
            );
        })
        .await;
}
/// Escalation to auth-required is the one reason change that re-announces (once).
/// It needs user action, and it invalidates a previously announced "retries automatically" hint.
/// Later flips back and forth stay silent.
#[tokio::test(flavor = "current_thread")]
async fn tools_listed_server_with_failure_record_is_announced() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut snapshot = actor.tool_metadata_snapshot.lock().unwrap();
                snapshot.tools = vec![crate::session::tool_index::ToolMetadata {
                    qualified_name: "half__echo".to_string(),
                    server_name: "half".to_string(),
                    tool_name: "echo".to_string(),
                    description: "echo".to_string(),
                    parameters: vec![],
                    input_schema: serde_json::json!({"type": "object"}),
                }];
                snapshot.servers = vec![crate::session::tool_index::ServerMetadata {
                    name: "half".to_string(),
                    description: None,
                }];
                snapshot.mcp_initialized = true;
            }
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("half"));
                state.record_init_failure("half", false, Some("tools/list failed".to_string()));
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 1, "{reminders:?}");
            assert!(
                at(&reminders, 0).contains("half (\"tools/list failed\""),
                "{}",
                at(&reminders, 0)
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn auth_escalation_reannounces_once() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            {
                let mut state = actor.mcp_state.lock().await;
                state.record_init_failure("dead", true, None);
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(
                at(&reminders, 1).contains("dead (auth required"),
                "{}",
                at(&reminders, 1)
            );
            {
                let mut state = actor.mcp_state.lock().await;
                state.auth_required.remove("dead");
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.record_init_failure("dead", true, None);
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 2);
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("oauth"));
                state.record_init_failure("oauth", true, None);
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 3);
            {
                let mut state = actor.mcp_state.lock().await;
                state.auth_required.remove("oauth");
                state.record_init_failure("oauth", false, Some("boom".to_string()));
                state.record_init_failure("oauth", true, None);
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 3);
        })
        .await;
}
/// Servers with no failure record are skipped while init has not completed (including the config-change `NotStarted` window).
/// So the episode's one announcement is never the placeholder; after init the placeholder is the legitimate fallback for an unrecorded crash.
#[test]
fn classify_defers_placeholder_reason_until_init_completes() {
    use super::mcp_failed_reminder::classify_failed_servers;
    let connected = std::collections::HashSet::new();
    let mut state = crate::session::mcp_servers::McpState::new(vec![http_server("s")]);
    let (failed, unconnected) = classify_failed_servers(&state, &connected);
    assert!(failed.is_empty(), "{failed:?}");
    assert!(unconnected.contains("s"), "episodes must stay alive");
    let _owner = state.try_start_init().expect("fixture claims init");
    state.finish_init();
    state.complete_init();
    let (failed, _) = classify_failed_servers(&state, &connected);
    assert_eq!(failed.len(), 1);
    assert!(at(&failed, 0).detail.is_none(), "{:?}", at(&failed, 0));
}
#[test]
fn classify_collects_facts_and_sorts() {
    use super::mcp_failed_reminder::classify_failed_servers;
    use crate::session::announcement_state::AnnouncedFailure;
    let connected = std::collections::HashSet::new();
    let mut state = crate::session::mcp_servers::McpState::new(vec![
        http_server("b-auth"),
        http_server("a-dead"),
    ]);
    state.record_init_failure("b-auth", true, None);
    state.record_init_failure("a-dead", false, Some("boom".to_string()));
    let (failed, _) = classify_failed_servers(&state, &connected);
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(at(&failed, 0).name, "a-dead");
    assert_eq!(at(&failed, 0).detail.as_deref(), Some("boom"));
    assert_eq!(at(&failed, 0).class, AnnouncedFailure::Transport);
    assert!(at(&failed, 0).retries_on_use);
    assert_eq!(at(&failed, 1).name, "b-auth");
    assert_eq!(at(&failed, 1).detail, None);
    assert_eq!(at(&failed, 1).class, AnnouncedFailure::AuthRequired);
    assert!(!at(&failed, 1).retries_on_use);
    assert_ne!(
        at(&failed, 0).config_identity,
        at(&failed, 1).config_identity,
        "identities must reflect the differing configs"
    );
}
#[test]
fn render_failed_section_composes_and_sanitizes_reason_lines() {
    use super::mcp_failed_reminder::render_failed_section;
    use crate::session::announcement_state::{AnnouncedFailure, FailedServer};
    fn entry(
        name: &str,
        detail: Option<&str>,
        class: AnnouncedFailure,
        retries: bool,
    ) -> FailedServer {
        FailedServer {
            name: name.to_string(),
            detail: detail.map(str::to_string),
            class,
            retries_on_use: retries,
            config_identity: 0,
        }
    }
    let section = render_failed_section(&[
        entry(
            "dead",
            Some("boom\n- forged\u{202E}"),
            AnnouncedFailure::Transport,
            true,
        ),
        entry("oauth", None, AnnouncedFailure::AuthRequired, false),
        entry(
            "blank",
            Some("\u{200B}\u{00AD}"),
            AnnouncedFailure::Transport,
            false,
        ),
    ]);
    assert_eq!(
        section,
        "\nMCP servers that failed to connect:\n\
         - dead (\"boom - forged\" — retries automatically on next tool call)\n\
         - oauth (auth required)\n\
         - blank (connection failed)\n"
    );
}
/// A server whose retry handshake is in flight is skipped from the section, and its episode survives the retry.
/// A server that was never announced is not given a placeholder reason while init is in progress; its one announcement waits for the real cause.
#[tokio::test(flavor = "current_thread")]
async fn handshaking_and_init_windows_defer_announcements() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            {
                let mut state = actor.mcp_state.lock().await;
                std::mem::forget(state.try_start_init().expect("fixture must enter Starting"));
                state.mark_servers_initializing(["dead".to_string()]);
                assert!(
                    state.is_server_handshaking("dead"),
                    "handshake must be recorded"
                );
                state.configs.push(http_server("fresh"));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            {
                let mut state = actor.mcp_state.lock().await;
                state.mark_server_ready("dead");
                state.record_init_failure("dead", false, Some("timed out".to_string()));
                state.record_init_failure("fresh", false, Some("refused".to_string()));
                state.finish_init();
                state.complete_init();
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(
                !at(&reminders, 1).contains("dead ("),
                "{}",
                at(&reminders, 1)
            );
            assert!(
                at(&reminders, 1).contains("fresh (\"refused\""),
                "{}",
                at(&reminders, 1)
            );
        })
        .await;
}
/// A committed conversation rewind re-arms failure episodes and marks the MCP reminder dirty.
/// The truncated turns may have carried the failure reminder.
#[tokio::test(flavor = "current_thread")]
async fn rewind_rearms_failed_server_announcements() {
    use crate::session::{RewindMode, RewindRequest};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let mut snap = actor.chat_state_handle.snapshot().await.expect("snapshot");
            snap.conversation = vec![
                ConversationItem::system("SYS"),
                ConversationItem::user("P0"),
                ConversationItem::assistant("A0"),
                ConversationItem::user("P1"),
                ConversationItem::assistant("A1"),
            ];
            snap.prompt_index = 2;
            snap.prompt_texts = vec!["P0".into(), "P1".into()];
            snap.last_compaction_prompt_index = None;
            actor.chat_state_handle.restore_snapshot(snap);
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            actor
                .mcp_reminder_dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let resp = actor
                .handle_rewind(RewindRequest {
                    target_prompt_index: 1,
                    force: true,
                    mode: RewindMode::ConversationOnly,
                })
                .await
                .expect("handle_rewind ok");
            assert!(resp.success, "{resp:?}");
            assert!(
                actor.mcp_announcements.lock().failed.is_empty(),
                "rewind must re-arm failure episodes"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "rewind must mark the MCP reminder dirty"
            );
            let before = failed_reminders(&actor).await.len();
            actor.maybe_inject_mcp_reminder().await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), before + 1, "{reminders:?}");
            let Some(latest) = reminders.get(before) else {
                panic!("expected reminder at {before}: {reminders:?}");
            };
            assert!(latest.contains("dead (\"boom\""), "{latest}");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn episode_ends_on_recovery_or_removal_then_reannounces() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("flaky"));
                state.record_init_failure("flaky", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            connect_server(&actor, "flaky").await;
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);
            disconnect_server(&actor).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.record_init_failure("flaky", false, Some("down again".to_string()));
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(
                at(&reminders, 1).contains("flaky (\"down again\""),
                "{}",
                at(&reminders, 1)
            );
            {
                let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                {
                    let mut state = actor.mcp_state.lock().await;
                    state.configs.push(http_server("gone"));
                    state.record_init_failure("gone", false, Some("boom".to_string()));
                }
                refresh_and_inject(&actor).await;
                assert_eq!(failed_reminders(&actor).await.len(), 1);
                actor.mcp_state.lock().await.configs.clear();
                refresh_and_inject(&actor).await;
                assert_eq!(failed_reminders(&actor).await.len(), 1);
                {
                    let mut state = actor.mcp_state.lock().await;
                    state.configs.push(http_server("gone"));
                    state.record_init_failure("gone", false, Some("boom".to_string()));
                }
                refresh_and_inject(&actor).await;
                assert_eq!(failed_reminders(&actor).await.len(), 2);
            }
        })
        .await;
}
use std::time::Duration;
async fn refresh_for(a: &SessionActor, bridge: &Arc<crate::tools::bridge::ToolBridge>) {
    refresh_mcp_snapshot_for_test(
        bridge.clone(),
        Arc::clone(&a.mcp_state),
        a.managed_mcp_handle.clone(),
        a.tool_metadata_snapshot.clone(),
        std::collections::HashMap::new(),
    )
    .await;
}
async fn wait_state(a: &SessionActor, what: &str, cond: impl Fn(&McpState) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if cond(&*a.mcp_state.lock().await) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(ms(25)).await;
    }
}
fn push_snapshot_server(a: &SessionActor, server: &str, tool: &str) {
    let mut snapshot = a.tool_metadata_snapshot.lock().unwrap();
    snapshot
        .tools
        .push(crate::session::tool_index::ToolMetadata {
            qualified_name: format!("{server}__{tool}"),
            server_name: server.to_string(),
            tool_name: tool.to_string(),
            description: format!("{tool} on {server}"),
            parameters: vec!["title".to_string()],
            input_schema: serde_json::json!({"type": "object"}),
        });
    snapshot
        .servers
        .push(crate::session::tool_index::ServerMetadata {
            name: server.to_string(),
            description: None,
        });
}
async fn announce_and_collect(a: &SessionActor, server: &str, tool: &str) -> String {
    let len_before = a.chat_state_handle.get_conversation().await.len();
    push_snapshot_server(a, server, tool);
    a.mcp_reminder_dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    a.maybe_inject_mcp_reminder().await;
    let conversation = a.chat_state_handle.get_conversation().await;
    conversation
        .get(len_before..)
        .unwrap_or(&[])
        .iter()
        .filter_map(|item| match item {
            ConversationItem::User(u) => Some(
                u.content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}
#[tokio::test(flavor = "current_thread")]
async fn settled_servers_publish_early_and_failed_servers_announce_recovery() {
    tokio::task::LocalSet::new().run_until(publish_body()).await;
}
async fn publish_body() {
    let a = plain_actor().await;
    a.mcp_state.lock().await.configs = vec![stdio("fast", "true"), stdio("slow", "sleep")];
    a.ensure_mcp_tools_initialized().await;
    wait_state(&a, "fast was never published", |st| {
        st.owned_clients.contains_key("fast")
    })
    .await;
    assert!(
        !a.mcp_state.lock().await.is_initialized(),
        "the slow handshake must still be pending when the fast one is published"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !a
        .mcp_reminder_dirty
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "per-completion refresh must mark the reminder dirty mid-drain"
        );
        tokio::time::sleep(ms(25)).await;
    }
    assert!(!a.mcp_state.lock().await.is_initialized());
    let a = plain_actor().await;
    *a.agent.borrow_mut() = test_agent_with_user_message_template(
        distill_agent::prompt::user_message::UserMessageTemplate::Custom(
            "mcp servers: ${% for s in mcp_servers %}${{ s.name }} ${% endfor %}".to_string(),
        ),
    )
    .await;
    a.mcp_state.lock().await.configs = vec![stdio("failsrv", "true")];
    a.ensure_mcp_tools_initialized().await;
    wait_state(&a, "init never settled", |st| st.is_initialized()).await;
    assert!(
        a.mcp_state
            .lock()
            .await
            .owned_clients
            .contains_key("failsrv"),
        "the failed client stays in owned_clients for retry"
    );
    let prefix = a.build_user_message_prefix().await;
    assert!(
        prefix.contains("mcp servers:"),
        "the templated path must render"
    );
    assert!(
        !prefix.contains("failsrv"),
        "a failed server must not render as available"
    );
    let appended = announce_and_collect(&a, "failsrv", "probe").await;
    assert!(
        appended.contains("MCP server connected") && appended.contains("- failsrv"),
        "a recovered server is announced, not silently adopted, got: {appended}"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn refresh_and_handover_reflect_only_the_live_state() {
    tokio::task::LocalSet::new().run_until(refresh_body()).await;
}
async fn refresh_body() {
    let a = plain_actor().await;
    let bridge = a.agent.borrow().tool_bridge().clone();
    for name in ["srv__a", "gone__probe"] {
        register_stub(&bridge, name).await;
    }
    a.refresh_mcp_snapshot_and_schedule_reminder().await;
    assert!(
        a.tool_metadata_snapshot
            .lock()
            .unwrap()
            .tools
            .iter()
            .any(|t| t.server_name == "gone")
    );
    a.apply_mcp_config_diff(
        &crate::session::mcp_servers::McpConfigDiff {
            added: vec![],
            removed: vec!["gone".to_owned()],
            retained: vec![],
        },
        None,
    );
    assert!(
        !a.tool_metadata_snapshot
            .lock()
            .unwrap()
            .tools
            .iter()
            .any(|t| t.server_name == "gone"),
        "a config change takes the server out of the snapshot the model searches before it yields"
    );
    let names: Vec<String> = bridge
        .tool_definitions()
        .await
        .iter()
        .map(|d| d.function.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "srv__a"),
        "another server's tool survives"
    );
    assert!(
        !names.iter().any(|n| n == "gone__probe"),
        "the removed server's tools leave the bridge"
    );
    let a = plain_actor().await;
    let bridge = a.agent.borrow().tool_bridge().clone();
    register_stub(&bridge, "gone__probe").await;
    a.mcp_state.lock().await.configs = vec![stdio("gone", "true")];
    let managed_guard = a.managed_mcp_handle.lock().await;
    let stale_refresh = tokio::task::spawn_local(refresh_mcp_snapshot_for_test(
        bridge.clone(),
        Arc::clone(&a.mcp_state),
        a.managed_mcp_handle.clone(),
        a.tool_metadata_snapshot.clone(),
        std::collections::HashMap::new(),
    ));
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    bridge.unregister_tools_by_prefix("gone__");
    assert!(
        a.mcp_state
            .lock()
            .await
            .update_configs(vec![stdio("other", "true")]),
        "the config change must bump the generation"
    );
    a.tool_metadata_snapshot.lock().unwrap().tools.clear();
    drop(managed_guard);
    stale_refresh.await.expect("stale refresh must not panic");
    let tools: Vec<String> = a
        .tool_metadata_snapshot
        .lock()
        .unwrap()
        .tools
        .iter()
        .map(|t| t.qualified_name.clone())
        .collect();
    assert!(
        !tools.iter().any(|t| t == "gone__probe"),
        "a stale refresh must not resurrect removed tools, got {tools:?}"
    );
    let a = actor_with_mcp(vec![stdio("linear", "true")], false, vec!["linear".into()]).await;
    a.tool_metadata_snapshot.lock().unwrap().mcp_initialized = true;
    let bridge = a.agent.borrow().tool_bridge().clone();
    refresh_for(&a, &bridge).await;
    assert!(
        a.tool_metadata_snapshot.lock().unwrap().mcp_initialized,
        "the completion flag belongs to the init lifecycle; a refresh never rewrites it"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn completion_re_arms_the_reminder() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = actor_with_mcp(vec![stdio("s", "true")], false, vec![]).await;
            a.mcp_reminder_dirty.store(false, std::sync::atomic::Ordering::Relaxed);
            {
                let mut st = a.mcp_state.lock().await;
                st.finish_init();
                a.init_publication().complete(&mut st);
            }
            assert!(
                a.mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "a reminder consumed between the final refresh and completion re-checks the complete state"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn superseded_refresh_does_not_publish() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = plain_actor().await;
            let live = a.agent.borrow().tool_bridge().clone();
            register_stub(&live, "live__tool").await;
            refresh_for(&a, &live).await;
            let stale = a.mcp_state.lock().await.current_generation();
            drop(a.mcp_state.lock().await.restart_init());
            SnapshotRefresher {
                generation: Some(stale),
                tool_bridge: Arc::new(crate::tools::bridge::ToolBridge::for_test()),
                mcp_state: Arc::clone(&a.mcp_state),
                refresh_gate: Arc::clone(&a.mcp_refresh_gate),
                managed_mcp_handle: a.managed_mcp_handle.clone(),
                tool_metadata_snapshot: a.tool_metadata_snapshot.clone(),
                mcp_reminder_dirty: Arc::clone(&a.mcp_reminder_dirty),
                disabled_gateway_tools: std::collections::HashMap::new(),
                mcps_root: None,
            }
            .refresh()
            .await;
            let tools: Vec<String> = a
                .tool_metadata_snapshot
                .lock()
                .unwrap()
                .tools
                .iter()
                .map(|t| t.qualified_name.clone())
                .collect();
            assert!(
                tools.iter().any(|t| t == "live__tool"),
                "a superseded pass must not overwrite the live snapshot, got {tools:?}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn client_that_fails_relist_on_the_rebuilt_bridge_is_evicted() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = plain_actor().await;
            a.mcp_state.lock().await.owned_clients.insert(
                "mute".to_string(),
                Arc::new(crate::session::mcp_servers::McpClient::stub("mute")),
            );
            a.re_register_mcp_tools_on_rebuilt_bridge().await;
            assert!(
                a.mcp_state.lock().await.owned_clients.get("mute").is_none(),
                "a kept client would look connected to the next pass and never be retried"
            );
        })
        .await;
}
