// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn mcp_init_progress_overwrites_the_counts() {
        let mut app = make_app_with_agent("sess-1");
        handle_ext_notification(&make_mcp_init_progress_notif(5, 0), &mut app);

        let changed = handle_ext_notification(&make_mcp_init_progress_notif(5, 2), &mut app);
        assert!(changed);

        let progress = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((progress.total, progress.connected), (5, 2));
    }

    #[test]
    fn server_status_routes_to_owning_agent() {
        use crate::views::extensions_modal::TabDataState;
        use crate::views::mcps_modal::McpServerDisplayStatus;
        use distill_shell::extensions::mcp::McpServerStatus;

        // Agent 0 owns sess-owner; Agent 1 is foregrounded.
        let mut app = make_app_two_agents();
        seed_owner_agent_with_open_modal(&mut app);
        // Give the active agent its own modal so we can prove it's untouched.
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.extensions_modal = Some(make_mcps_modal_with_servers(vec![
                crate::views::mcps_modal::McpServerInfo {
                    name: "alpha".into(),
                    display_name: None,
                    status: McpServerDisplayStatus::Initializing,
                    tool_count: 0,
                    auth_required: false,
                    setup_required: false,
                    setup: None,
                    setup_values: std::collections::HashMap::new(),
                    tools: Vec::new(),
                    enabled: true,
                    source: "local".into(),
                    blocked_reason: None,
                    wire_source: crate::views::mcps_modal::McpWireSource::Local,
                    plugin_name: None,
                    is_managed_gateway: false,
                },
            ]));
        }

        let tools = Some(serde_json::json!([
            { "name": "t1", "description": "one", "enabled": true },
            { "name": "t2", "enabled": true },
        ]));
        let notif = make_server_status_notif("sess-owner", "alpha", McpServerStatus::Ready, tools);
        let redraw = handle_mcp_server_status(&notif, &mut app);
        assert!(
            !redraw,
            "owner is background — mutation must not request a redraw"
        );

        // The owner's modal was mutated
        let owner_modal = app
            .agents
            .get(&AgentId(0))
            .unwrap()
            .extensions_modal
            .as_ref()
            .unwrap();
        let TabDataState::Loaded(ref owner_servers) = owner_modal.mcps_data else {
            panic!("owner modal must still be in Loaded state");
        };
        let Some(owner) = owner_servers.first() else {
            panic!("expected an owner server");
        };
        assert_eq!(owner.status, McpServerDisplayStatus::Ready);
        assert_eq!(owner.tool_count, 2);
        assert_eq!(owner.tools.len(), 2);

        // Active agent's modal must be untouched.
        let active_modal = app
            .agents
            .get(&AgentId(1))
            .unwrap()
            .extensions_modal
            .as_ref()
            .unwrap();
        let TabDataState::Loaded(ref active_servers) = active_modal.mcps_data else {
            panic!("active modal must still be in Loaded state");
        };
        let Some(active) = active_servers.first() else {
            panic!("expected an active server");
        };
        assert_eq!(
            active.status,
            McpServerDisplayStatus::Initializing,
            "active-view agent must not absorb the owning agent's push"
        );
    }

    #[test]
    fn mcp_init_progress_creates_when_none() {
        // The first init_progress creates McpInitProgress
        let mut app = make_app_with_agent("sess-1");
        assert!(test_agent(&app, AgentId(0)).mcp_init_progress.is_none());

        let notif = make_mcp_init_progress_notif(3, 1);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(changed);

        let progress = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!(progress.total, 3);
        assert_eq!(progress.connected, 1);
    }

    #[test]
    fn mcp_initialized_clears_progress() {
        // x.ai/mcp_initialized must set mcp_init_progress to None.
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.mcp_init_progress = Some(crate::app::agent_view::McpInitProgress {
            total: 3,
            connected: 3,
        });

        let notif = make_mcp_initialized_notif("sess-1");
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(changed);
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_none(),
            "mcp_initialized must clear mcp_init_progress",
        );
    }

    #[test]
    fn mcp_full_lifecycle_creates_then_clears() {
        // Full N-server lifecycle:
        //   init_progress(0/3) → init_progress(2/3)
        //   → init_progress(3/3) → mcp_initialized → None
        let mut app = make_app_with_agent("sess-1");
        assert!(test_agent(&app, AgentId(0)).mcp_init_progress.is_none());

        // The shell reports the real count
        handle_ext_notification(&make_mcp_init_progress_notif(3, 0), &mut app);
        let p = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((p.total, p.connected), (3, 0));

        // Incremental progress.
        handle_ext_notification(&make_mcp_init_progress_notif(3, 2), &mut app);
        let p = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((p.total, p.connected), (3, 2));

        // All connected.
        handle_ext_notification(&make_mcp_init_progress_notif(3, 3), &mut app);
        let p = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((p.total, p.connected), (3, 3));

        // mcp_initialized clears everything.
        handle_ext_notification(&make_mcp_initialized_notif("sess-1"), &mut app);
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_none(),
            "mcp_initialized must clear progress after full lifecycle",
        );
    }

    #[test]
    fn mcp_zero_server_lifecycle() {
        // 0-server lifecycle (the bug scenario):
        //   init_progress(0/0) → mcp_initialized → None
        // Previously mcp_initialized was never sent for 0 servers, leaving a stuck progress indicator
        let mut app = make_app_with_agent("sess-1");
        assert!(test_agent(&app, AgentId(0)).mcp_init_progress.is_none());

        // The shell sends 0/0 for the 0-server case
        handle_ext_notification(&make_mcp_init_progress_notif(0, 0), &mut app);
        let p = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((p.total, p.connected), (0, 0));

        // The shell now sends mcp_initialized even with 0 servers
        handle_ext_notification(&make_mcp_initialized_notif("sess-1"), &mut app);
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_none(),
            "0-server mcp_initialized must clear progress",
        );
    }

    #[test]
    fn mcp_init_progress_routes_to_background_session() {
        // init_progress carrying a background session's sessionId must update *that* agent's indicator, not the foregrounded one
        // It must not force a redraw (the background spinner isn't visible)
        let mut app = make_app_with_agent("sess-A");
        app.agents.insert(AgentId(1), make_agent(Some("sess-B")));

        let notif = make_mcp_init_progress_notif_for(4, 1, "sess-B");
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            !changed,
            "background-session progress must not force a redraw"
        );

        let bg = test_agent(&app, AgentId(1)).mcp_init_progress.as_ref().unwrap();
        assert_eq!((bg.total, bg.connected), (4, 1));
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_none(),
            "foreground agent must be untouched by a background session's progress",
        );
    }

    #[test]
    fn mcp_initialized_routes_to_background_session() {
        // mcp_initialized for a background session must clear *that* agent's indicator while leaving the foreground agent's intact
        // Previously the clear was applied to whichever agent was active, so a background agent's spinner could stick forever
        let mut app = make_app_with_agent("sess-A");
        app.agents.insert(AgentId(1), make_agent(Some("sess-B")));
        for id in [AgentId(0), AgentId(1)] {
            app.agents.get_mut(&id).unwrap().mcp_init_progress =
                Some(crate::app::agent_view::McpInitProgress {
                    total: 2,
                    connected: 0,
                });
        }

        let notif = make_mcp_initialized_notif_for("sess-B");
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            !changed,
            "clearing a background spinner must not force a redraw",
        );
        assert!(
            test_agent(&app, AgentId(1)).mcp_init_progress.is_none(),
            "background session's spinner must be cleared",
        );
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_some(),
            "foreground agent's spinner must NOT be cleared by another session",
        );
    }

    #[test]
    fn mcp_initialized_unknown_session_is_dropped() {
        // An mcp_initialized for a session that matches no agent must not clear anyone's indicator (no misrouting to the active agent)
        let mut app = make_app_with_agent("sess-A");
        app.agents.get_mut(&AgentId(0)).unwrap().mcp_init_progress =
            Some(crate::app::agent_view::McpInitProgress {
                total: 1,
                connected: 0,
            });

        let notif = make_mcp_initialized_notif_for("sess-unknown");
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(!changed);
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_some(),
            "unknown-session mcp_initialized must not clear the active agent",
        );
    }

    #[test]
    fn mcp_lifecycle_notif_for_subagent_session_is_dropped() {
        // A subagent runs its own MCP init, emitting init_progress and mcp_initialized under the *child* session id
        // Those must NOT write to or clear the parent agent's mcp_init_progress
        // The indicator belongs to the root agent and has no subagent slot, so a subagent's init must not clobber the parent's spinner
        let mut app = make_app_with_agent("sess-A");
        app.agents.get_mut(&AgentId(0)).unwrap().mcp_init_progress =
            Some(crate::app::agent_view::McpInitProgress {
                total: 2,
                connected: 1,
            });
        // Register a subagent child view keyed by the child session id.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .insert_test_child(
                "child-sess".to_string(),
                Box::new(make_agent(Some("child-sess"))),
            );

        // init_progress for the child session must leave the parent untouched.
        let changed = handle_ext_notification(
            &make_mcp_init_progress_notif_for(5, 0, "child-sess"),
            &mut app,
        );
        assert!(
            !changed,
            "subagent init_progress must not redraw the parent"
        );
        let p = test_agent(&app, AgentId(0)).mcp_init_progress.as_ref().unwrap();
        assert_eq!(
            (p.total, p.connected),
            (2, 1),
            "parent spinner must be untouched by a subagent's init",
        );

        // mcp_initialized for the child session must not clear the parent.
        let changed =
            handle_ext_notification(&make_mcp_initialized_notif_for("child-sess"), &mut app);
        assert!(!changed);
        assert!(
            test_agent(&app, AgentId(0)).mcp_init_progress.is_some(),
            "subagent mcp_initialized must not clear the parent's spinner",
        );
    }

    #[test]
    fn server_status_handler_noop_when_modal_closed_background() {
        use distill_shell::extensions::mcp::McpServerStatus;
        let mut app = make_app_two_agents();
        // The owner is background and has NO modal open
        // server_status must be a silent no-op (no Effect scheduling, no redraw)
        let notif = make_server_status_notif("sess-owner", "alpha", McpServerStatus::Ready, None);
        let redraw = handle_mcp_server_status(&notif, &mut app);
        assert!(!redraw, "closed-modal cheap path must not request a redraw");
        assert!(
            app.pending_effects.is_empty(),
            "closed-modal cheap path must not schedule any effects"
        );
        assert!(
            app.agents
                .get(&AgentId(0))
                .unwrap()
                .extensions_modal
                .is_none(),
            "owner modal must remain closed"
        );
    }

    /// Pin the cheap-path behavior for the FOREGROUND case too.
    /// Without this test, the background case passes trivially regardless of how the cheap path is gated on `is_active`.
    #[test]
    fn server_status_handler_noop_when_modal_closed_foreground() {
        use distill_shell::extensions::mcp::McpServerStatus;
        let mut app = make_app_two_agents();
        // The foreground is agent 1 (sess-active)
        // Send a push targeting the foregrounded agent, with no modal open
        let notif = make_server_status_notif("sess-active", "alpha", McpServerStatus::Ready, None);
        let redraw = handle_mcp_server_status(&notif, &mut app);
        assert!(
            !redraw,
            "foreground + closed-modal must still be a no-op (no row → no redraw)"
        );
        assert!(
            app.pending_effects.is_empty(),
            "server_status NEVER schedules an effect (push is per-row, not list-refetch)"
        );
    }

    /// Pin the cheap-path behavior when the modal is open but the data is still loading.
    /// The `TabDataState::Loaded` gate must return early without panicking.
    #[test]
    fn server_status_handler_noop_when_modal_data_still_loading() {
        use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab, TabDataState};
        use distill_shell::extensions::mcp::McpServerStatus;
        let mut app = make_app_two_agents();
        {
            let owner = app.agents.get_mut(&AgentId(0)).unwrap();
            let mut modal = ExtensionsModalState::new(ExtensionsTab::McpServers);
            modal.mcps_data = TabDataState::Loading;
            owner.extensions_modal = Some(modal);
        }
        let notif = make_server_status_notif("sess-owner", "alpha", McpServerStatus::Ready, None);
        let redraw = handle_mcp_server_status(&notif, &mut app);
        assert!(!redraw, "Loading state must skip the patch");
        assert!(app.pending_effects.is_empty());
    }

    /// A malformed `status` field must NOT silently coerce to `Unavailable`.
    /// The whole payload must fail to parse and log a warning instead.
    #[test]
    fn server_status_malformed_status_does_not_silently_repaint() {
        use crate::views::extensions_modal::TabDataState;
        use crate::views::mcps_modal::McpServerDisplayStatus;
        let mut app = make_app_two_agents();
        seed_owner_agent_with_open_modal(&mut app);
        // Send a payload with the `status` field missing entirely; a defaulting decoder would silently coerce this to Unavailable
        let payload = serde_json::json!({
            "sessionId": "sess-owner",
            "name": "alpha",
            "source": "local",
            "reason": "initialized",
            // Deliberately no `status`
        });
        let raw = serde_json::value::to_raw_value(&payload).unwrap();
        let notif = acp::ExtNotification::new("x.ai/mcp/server_status", raw.into());
        let redraw = handle_mcp_server_status(&notif, &mut app);
        assert!(!redraw, "malformed payload must not request a redraw");

        let modal = app
            .agents
            .get(&AgentId(0))
            .unwrap()
            .extensions_modal
            .as_ref()
            .unwrap();
        let TabDataState::Loaded(ref servers) = modal.mcps_data else {
            panic!("modal still Loaded");
        };
        let Some(server) = servers.first() else {
            panic!("expected a server");
        };
        assert_eq!(
            server.status,
            McpServerDisplayStatus::Initializing,
            "malformed status must NOT silently map to Unavailable"
        );
    }

    /// A `tools` field that is present but not an array must drop ONLY the tools update.
    /// The `status` field still applies; we do not drop the whole push.
    #[test]
    fn server_status_lenient_tools_decoding_still_applies_status() {
        use crate::views::extensions_modal::TabDataState;
        use crate::views::mcps_modal::McpServerDisplayStatus;
        use distill_shell::extensions::mcp::McpServerStatus;
        let mut app = make_app_two_agents();
        seed_owner_agent_with_open_modal(&mut app);

        // tools is an arbitrary non-array shape; a future shell might emit something like `{ added: [], removed: [] }`
        // It must NOT take down the status update
        let bad_tools = Some(serde_json::json!({"added": [], "removed": []}));
        let notif =
            make_server_status_notif("sess-owner", "alpha", McpServerStatus::Ready, bad_tools);
        let _ = handle_mcp_server_status(&notif, &mut app);

        let modal = app
            .agents
            .get(&AgentId(0))
            .unwrap()
            .extensions_modal
            .as_ref()
            .unwrap();
        let TabDataState::Loaded(ref servers) = modal.mcps_data else {
            panic!("modal still Loaded");
        };
        let Some(server) = servers.first() else {
            panic!("expected a server");
        };
        assert_eq!(
            server.status,
            McpServerDisplayStatus::Ready,
            "malformed tools must not take down the status update"
        );
        // tool_count and tools were preserved (we dropped the tools update rather than overwriting with empty)
        assert_eq!(server.tool_count, 0);
        assert!(server.tools.is_empty());
    }

    /// Pin that the pager deserializes against the *shell's* `McpServerStatus` enum, so a new variant needs no pager change to be recognized.
    /// Round-trip through `serde_json::to_string` of the shell type itself.
    #[test]
    fn server_status_round_trips_shell_canonical_type() {
        use distill_shell::extensions::mcp::{
            McpServerSource, McpServerStatus, McpServerStatusPayload, McpServerStatusReason,
        };
        let payload = McpServerStatusPayload {
            session_id: "s".into(),
            name: "alpha".into(),
            source: McpServerSource::Local,
            status: McpServerStatus::NeedsAuth,
            reason: McpServerStatusReason::AuthExpired,
            detail: Some("token expired".into()),
            tools: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let roundtripped: McpServerStatusPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, roundtripped);
        // `needsAuth` must be on the wire as the lowercase form, not mixed-case; this verifies the rename_all = lowercase contract
        assert!(
            json.contains("\"needsauth\""),
            "wire form must be lowercase 'needsauth'; got {json}"
        );
    }

    /// `servers_updated` has NO `sessionId` on the wire.
    /// The handler must broadcast to every agent with an open modal, NOT fall through to `active_view`.
    #[test]
    fn servers_updated_broadcasts_to_every_agent_with_open_modal() {
        let mut app = make_app_two_agents();
        // Open modals on BOTH agents; the broadcast must hit both
        seed_owner_agent_with_open_modal(&mut app);
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.extensions_modal = Some(make_mcps_modal_with_servers(vec![
                crate::views::mcps_modal::McpServerInfo {
                    name: "beta".into(),
                    display_name: None,
                    status: crate::views::mcps_modal::McpServerDisplayStatus::Initializing,
                    tool_count: 0,
                    auth_required: false,
                    setup_required: false,
                    setup: None,
                    setup_values: std::collections::HashMap::new(),
                    tools: Vec::new(),
                    enabled: true,
                    source: "local".into(),
                    blocked_reason: None,
                    wire_source: crate::views::mcps_modal::McpWireSource::Local,
                    plugin_name: None,
                    is_managed_gateway: false,
                },
            ]));
        }

        let notif = make_servers_updated_notif();
        let redraw = handle_mcp_servers_updated(&notif, &mut app);

        assert!(
            redraw,
            "active agent had its modal open — redraw must be requested"
        );
        assert_eq!(
            app.pending_effects.len(),
            2,
            "servers_updated must broadcast: one FetchMcpsList per agent with an open modal"
        );
        let mut targets: Vec<usize> = app
            .pending_effects
            .iter()
            .filter_map(|e| match e {
                Effect::FetchMcpsList {
                    agent_id, cache, ..
                } => {
                    assert!(*cache, "broadcast must use the debounced cache=true path");
                    Some(agent_id.0)
                }
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(targets, vec![0, 1]);
    }

    /// Agents without an open modal must NOT receive a refetch (cheap path) even though they are eligible to receive the broadcast in principle.
    #[test]
    fn servers_updated_skips_agents_with_closed_modal() {
        let mut app = make_app_two_agents();
        // Only agent 1 (foregrounded) has a modal.
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.extensions_modal = Some(make_mcps_modal_with_servers(Vec::new()));
        }
        let notif = make_servers_updated_notif();
        let _ = handle_mcp_servers_updated(&notif, &mut app);
        assert_eq!(
            app.pending_effects.len(),
            1,
            "agent 0 has no modal open — must NOT receive a refetch"
        );
        let Effect::FetchMcpsList { agent_id, .. } = app.pending_effects.first().unwrap() else {
            panic!();
        };
        assert_eq!(
            *agent_id,
            AgentId(1),
            "only the agent with an open modal gets the refetch"
        );
    }

    /// A second `servers_updated` push that lands before agent A's pending fetch drains must coalesce for agent A.
    /// It must still schedule a fresh fetch for agent B if B's first push was never seen.
    #[test]
    fn servers_updated_per_agent_coalescing() {
        let mut app = make_app_two_agents();
        seed_owner_agent_with_open_modal(&mut app);
        let notif = make_servers_updated_notif();
        let _ = handle_mcp_servers_updated(&notif, &mut app);
        assert_eq!(app.pending_effects.len(), 1);

        // Second push: the owner is now coalesced (agent 0 has a pending fetch)
        // We add a modal to agent 1; that agent's push must NOT be dropped just because agent 0 has a pending fetch
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.extensions_modal = Some(make_mcps_modal_with_servers(Vec::new()));
        }
        let _ = handle_mcp_servers_updated(&notif, &mut app);
        assert_eq!(
            app.pending_effects.len(),
            2,
            "agent 1's first push must schedule a fetch despite agent 0 having a pending one"
        );
        let mut targets: Vec<usize> = app
            .pending_effects
            .iter()
            .filter_map(|e| match e {
                Effect::FetchMcpsList { agent_id, .. } => Some(agent_id.0),
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(targets, vec![0, 1]);
    }

    /// The `mcp_initialized` wire shape is `{ sessionId, mcpToolCount, elapsedMs }`.
    /// sessionId routing applies; the matched agent's `mcp_init_progress` overlay must be cleared.
    #[test]
    fn mcp_initialized_clears_init_progress_on_owner() {
        use crate::app::agent_view::McpInitProgress;
        let mut app = make_app_two_agents();
        // Seed init progress on the OWNER (agent 0) and on the active view (agent 1)
        // The push must clear only the owner's overlay
        {
            let owner = app.agents.get_mut(&AgentId(0)).unwrap();
            owner.mcp_init_progress = Some(McpInitProgress {
                total: 5,
                connected: 3,
            });
        }
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.mcp_init_progress = Some(McpInitProgress {
                total: 5,
                connected: 3,
            });
        }

        let notif = make_mcp_initialized_notif_for("sess-owner");
        let _ = handle_mcp_tools_changed(&notif, &mut app);

        assert!(
            app.agents
                .get(&AgentId(0))
                .unwrap()
                .mcp_init_progress
                .is_none(),
            "owner's init progress overlay must be cleared"
        );
        assert!(
            app.agents
                .get(&AgentId(1))
                .unwrap()
                .mcp_init_progress
                .is_some(),
            "active-view agent's init progress must NOT be cleared by a background-agent init"
        );
    }

    #[test]
    fn tools_changed_post_h2_routes_to_owning_agent_not_active_view() {
        let mut app = make_app_two_agents();
        seed_owner_agent_with_open_modal(&mut app);
        // The active agent has NO modal, so a handler that routed by active view would do nothing
        // Routing by sessionId must schedule a fetch against the OWNER agent (agent 0)
        let notif = make_tools_changed_notif_post_h2("sess-owner");
        let _ = handle_mcp_tools_changed(&notif, &mut app);

        assert_eq!(
            app.pending_effects.len(),
            1,
            "tools_changed with sessionId must route to the owner"
        );
        let Effect::FetchMcpsList { agent_id, .. } =
            app.pending_effects.first().expect("effect scheduled")
        else {
            panic!("expected FetchMcpsList");
        };
        assert_eq!(
            *agent_id,
            AgentId(0),
            "tools_changed with sessionId must route to the owner, not the active view"
        );
    }

    /// Compatibility guard: older shells emit `{ serverName, tools }` with NO sessionId.
    /// The pager must fall back to `active_view`.

    #[test]
    fn tools_changed_pre_h2_falls_back_to_active_view() {
        let mut app = make_app_two_agents();
        // The active agent (agent 1) gets a modal; the owner (agent 0) does not
        {
            let active = app.agents.get_mut(&AgentId(1)).unwrap();
            active.extensions_modal = Some(make_mcps_modal_with_servers(Vec::new()));
        }
        let notif = make_tools_changed_notif_pre_h2();
        let _ = handle_mcp_tools_changed(&notif, &mut app);

        assert_eq!(
            app.pending_effects.len(),
            1,
            "legacy payload without sessionId falls back to active view"
        );
        let Effect::FetchMcpsList { agent_id, .. } = app.pending_effects.first().unwrap() else {
            panic!();
        };
        assert_eq!(
            *agent_id,
            AgentId(1),
            "legacy fallback targets the foregrounded agent"
        );
    }

