// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use distill_mcp::servers::{McpInitStrategy, SharedMcpState};
use std::time::Duration;

async fn timed<F: Future>(f: F) -> (F::Output, Duration) {
    let start = tokio::time::Instant::now();
    let out = f.await;
    (out, start.elapsed())
}

async fn pending_linear_actor(strategy: McpInitStrategy) -> SessionActor {
    let a = actor_with_mcp(vec![stdio("linear", "true")], false, vec!["linear".into()]).await;
    a.mcp_strategy.set(strategy);
    a
}

async fn prepare(a: &SessionActor, id: &str, tool: &str) -> Result<PreparedToolCall, ToolLoop> {
    let call = crate::sampling::types::ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(tool, "{}".to_string()),
    };
    let mut deferred = Vec::new();
    a.prepare_tool_call(call, &mut deferred)
        .await
        .expect("prepare")
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn first_prompt_proceeds_at_grace_expiry_not_the_slowest_server() {
    tokio::task::LocalSet::new().run_until(grace_body()).await;
}

async fn grace_body() {
    let no_configs = actor_with_mcp(vec![], false, vec![]).await;
    let ready = actor_with_mcp(vec![stdio("linear", "true")], true, vec![]).await;
    let (_, fast) = timed(async {
        no_configs.wait_for_mcp_startup_grace().await;
        ready.wait_for_mcp_startup_grace().await;
    })
    .await;
    assert!(
        fast < ms(50),
        "no configs / already ready skip the grace, took {fast:?}"
    );

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl distill_mcp::acp_transport::AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Err("unused".to_string())
        }
    }
    let acp_only = actor_with_mcp(vec![], false, vec!["sdk".into()]).await;
    acp_only.mcp_state.lock().await.set_acp_servers(
        vec![distill_mcp::servers::AcpServerEntry {
            name: "sdk".to_string(),
            server_id: "sdk-1".to_string(),
        }],
        Arc::new(NoopInvoker),
    );
    let (_, acp_wait) = timed(acp_only.wait_for_mcp_startup_grace()).await;
    assert!(
        acp_wait >= MCP_STARTUP_GRACE,
        "ACP-only pays the grace, got {acp_wait:?}"
    );

    let restarted = pending_linear_actor(McpInitStrategy::Blocking).await;
    let state = Arc::clone(&restarted.mcp_state);
    tokio::task::spawn_local(async move {
        tokio::time::sleep(MCP_STARTUP_GRACE / 2).await;
        assert!(
            state
                .lock()
                .await
                .update_configs(vec![stdio("other", "true")])
        );
        tokio::time::sleep(ms(10)).await;
        let mut st = state.lock().await;
        std::mem::forget(st.try_start_init().expect("reclaim after bump"));
        st.mark_servers_initializing(vec!["other".to_string()]);
    });
    let (_, restart_wait) = timed(restarted.wait_for_mcp_startup_grace()).await;
    assert!(
        restart_wait >= MCP_STARTUP_GRACE + MCP_STARTUP_GRACE / 2,
        "a bump mid-grace must wake the waiter and re-pay the fresh grace, got {restart_wait:?}"
    );
    assert!(
        restart_wait < MCP_STARTUP_GRACE * 2,
        "restarted grace stays bounded"
    );

    let a = pending_linear_actor(McpInitStrategy::Blocking).await;
    let (prefix, prefix_wait) =
        timed(a.build_prefix_after_mcp_wait(a.requires_full_mcp_wait())).await;
    assert!(
        prefix_wait < ms(1900),
        "Default prefix skips MCP waits, took {prefix_wait:?}"
    );
    assert!(
        !prefix.contains("linear"),
        "Default prefix carries no MCP content"
    );

    let ((_defs, first_ms), gate) = timed(a.prepare_tool_definitions_timed()).await;
    assert!(
        gate >= MCP_STARTUP_GRACE,
        "pending handshakes get the grace, got {gate:?}"
    );
    assert!(
        gate < MCP_STARTUP_GRACE + Duration::from_secs(3),
        "the gate is bounded"
    );
    assert!(first_ms >= MCP_STARTUP_GRACE.as_millis() as u64);

    let (_defs, second_ms) = a.prepare_tool_definitions_timed().await;
    assert!(
        second_ms < 200,
        "a later gate must not restart the grace, got {second_ms}ms"
    );

    a.mcp_state
        .lock()
        .await
        .update_configs(vec![stdio("other", "true")]);
    let (_defs, third_ms) = a.prepare_tool_definitions_timed().await;
    assert!(
        third_ms >= MCP_STARTUP_GRACE.as_millis() as u64,
        "a new generation gets a fresh grace, got {third_ms}ms"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn gate_re_owns_an_abandoned_init_instead_of_waiting_on_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            // The dropped owner had seeded an empty set and never completed.
            let a = plain_actor().await;
            {
                let mut st = a.mcp_state.lock().await;
                assert!(st.update_configs(vec![stdio("fast", "true")]));
                let owner = st.try_start_init().expect("claims init");
                st.mark_servers_initializing(std::iter::empty::<String>());
                drop(owner);
                assert!(st.is_init_abandoned());
            }
            a.wait_for_mcp_startup_grace().await;
            assert!(
                a.mcp_state.lock().await.has_finished_init(),
                "the gate re-owns an abandoned init rather than waiting on it"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn delivery_tool_sessions_wait_fully_under_progressive_strategy() {
    tokio::task::LocalSet::new()
        .run_until(progressive_body())
        .await;
}

async fn progressive_body() {
    let a = pending_linear_actor(McpInitStrategy::Progressive).await;
    *a.delivery_tools.borrow_mut() = vec!["linear__post".to_string()];

    let state = Arc::clone(&a.mcp_state);
    tokio::task::spawn_local(async move {
        tokio::time::sleep(ms(300)).await;
        state.lock().await.complete_init();
    });
    let (_prefix, prefix_wait) =
        timed(a.build_prefix_after_mcp_wait(a.requires_full_mcp_wait())).await;
    assert!(
        prefix_wait >= ms(300),
        "prefix gate waits under Progressive"
    );
    assert!(
        prefix_wait < Duration::from_secs(10),
        "settle wakes the gate"
    );

    // A fresh, still-running pass for the second gate.
    let _second_pass = {
        let mut st = a.mcp_state.lock().await;
        let claim = st.restart_init();
        st.mark_servers_initializing(["linear".to_owned()]);
        claim
    };
    let state = Arc::clone(&a.mcp_state);
    tokio::task::spawn_local(async move {
        tokio::time::sleep(ms(300)).await;
        let mut st = state.lock().await;
        st.finish_init();
        st.complete_init();
    });
    let ((_defs, wait_ms), gate) = timed(a.prepare_tool_definitions_timed()).await;
    assert!(
        gate >= ms(300),
        "definitions gate waits for full init under Progressive"
    );
    assert!(wait_ms >= 300, "the timed wait attributes the init wait");

    // `sleep` never speaks MCP, so the successor pass a rebuild starts below cannot settle early.
    let wedged = Arc::new(
        actor_with_mcp(vec![stdio("linear", "sleep")], false, vec!["linear".into()]).await,
    );
    wedged.mcp_strategy.set(McpInitStrategy::Progressive);
    *wedged.delivery_tools.borrow_mut() = vec!["linear__post".to_string()];
    let bg = Arc::clone(&wedged);
    let full_wait = wedged.requires_full_mcp_wait();
    wedged.deferred_prefix.arm(
        tokio::task::spawn_local(async move { bg.build_prefix_after_mcp_wait(full_wait).await }),
        full_wait,
    );
    let (_, deferred_wait) = timed(wedged.ensure_prefix_ready()).await;
    assert!(
        deferred_wait >= DELIVERY_TOOLS_DEFAULT_PREFIX_WAIT,
        "the deferred prefix runs the full delivery wait, got {deferred_wait:?}"
    );
    assert!(!wedged.chat_state_handle.get_conversation().await.is_empty());

    // A zero-turn rebuild rebuilds the prefix through the same policy, so a delivery-tools session still holds the wait.
    let (_, rebuild_wait) = timed(wedged.handle_rebuild_agent_for_definition(
        distill_agent::AgentDefinition::default_distill(),
        distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
    ))
    .await;
    assert!(
        rebuild_wait >= DELIVERY_TOOLS_DEFAULT_PREFIX_WAIT,
        "the rebuilt prefix runs the full delivery wait, got {rebuild_wait:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn delivery_policy_applied_after_the_deferred_build_started_still_waits_fully() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = Arc::new(
                actor_with_mcp(vec![stdio("linear", "sleep")], false, vec!["linear".into()]).await,
            );
            a.mcp_strategy.set(McpInitStrategy::Progressive);
            let bg = Arc::clone(&a);
            a.deferred_prefix.arm(
                tokio::task::spawn_local(async move {
                    bg.build_prefix_after_mcp_wait(/*full_wait*/ false).await
                }),
                false,
            );
            *a.delivery_tools.borrow_mut() = vec!["linear__post".to_string()];
            let (_, wait) = timed(a.ensure_prefix_ready()).await;
            assert!(
                wait >= DELIVERY_TOOLS_DEFAULT_PREFIX_WAIT,
                "the policy arriving late still holds the full wait, got {wait:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn delivery_wait_is_paid_once_per_generation_when_init_wedges() {
    tokio::task::LocalSet::new().run_until(budget_body()).await;
}

async fn budget_body() {
    let abandoned = actor_with_mcp(vec![stdio("linear", "true")], false, vec![]).await;
    drop(
        abandoned
            .mcp_state
            .lock()
            .await
            .try_start_init()
            .expect("claims init"),
    );
    tokio::time::timeout(
        Duration::from_secs(600),
        abandoned.wait_for_mcp_initialized(),
    )
    .await
    .expect("the full wait re-owns an abandoned init instead of waiting on it");

    let starter = actor_with_mcp(vec![stdio("slow", "sleep")], false, vec![]).await;
    starter.mcp_strategy.set(McpInitStrategy::Blocking);
    *starter.delivery_tools.borrow_mut() = vec!["slow__post".to_string()];
    let (_defs, starter_ms) = starter.prepare_tool_definitions_timed().await;
    assert!(
        starter_ms >= 5_000,
        "a self-started wait must not conclude at finish_init"
    );

    let superseded = actor_with_mcp(vec![stdio("slow2", "sleep")], false, vec![]).await;
    let stale_claim = superseded
        .mcp_state
        .lock()
        .await
        .try_start_init()
        .expect("stale pass claims");
    let wait = superseded.wait_for_mcp_initialized_bounded_once();
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut wait)
            .await
            .is_err()
    );
    assert!(
        superseded
            .mcp_state
            .lock()
            .await
            .update_configs(vec![stdio("slow3", "sleep")])
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut wait)
            .await
            .is_err(),
        "supersession must not conclude the wait"
    );
    assert!(
        superseded
            .mcp_state
            .write_if_owner(stale_claim, |st, _| st.cancel_any_init())
            .await
            .is_err(),
        "a stale claim cannot release the successor's init"
    );
    {
        let mut st = superseded.mcp_state.lock().await;
        assert!(st.is_initializing() && st.try_start_init().is_none());
    }
    assert!(!superseded.full_mcp_wait_timed_out());
    tokio::time::timeout(Duration::from_secs(200), &mut wait)
        .await
        .expect("the wait concludes at the new generation's settle");
    assert!(!superseded.full_mcp_wait_timed_out());

    let a = pending_linear_actor(McpInitStrategy::Blocking).await;
    *a.delivery_tools.borrow_mut() = vec!["linear__post".to_string()];
    {
        let wait = a.wait_for_mcp_initialized_bounded_once();
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), &mut wait)
                .await
                .is_err(),
            "the wedged wait must still be pending when dropped"
        );
    }
    let (_defs, first_ms) = a.prepare_tool_definitions_timed().await;
    assert!(
        first_ms >= 120_000,
        "the first concluded wait pays the full bound"
    );
    let (_defs, second_ms) = a.prepare_tool_definitions_timed().await;
    assert!(
        second_ms < 1_000,
        "a spent budget is not paid again, got {second_ms}ms"
    );
    {
        let mut st = a.mcp_state.lock().await;
        assert!(st.update_configs(vec![stdio("other", "true")]));
        std::mem::forget(st.try_start_init().expect("fresh generation claims"));
        st.mark_servers_initializing(vec!["other".to_string()]);
    }
    let (_defs, third_ms) = a.prepare_tool_definitions_timed().await;
    assert!(
        third_ms >= 120_000,
        "a fresh generation pays the bound again"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_dispatch_gates_on_its_own_server_not_the_slowest_handshake() {
    tokio::task::LocalSet::new()
        .run_until(dispatch_body())
        .await;
}

async fn dispatch_body() {
    struct ReadySdkServer;
    #[async_trait::async_trait]
    impl distill_mcp::acp_transport::AcpReverseInvoker for ReadySdkServer {
        async fn invoke(
            &self,
            _server_id: &str,
            message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            let id = message
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let result = match message.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => serde_json::json!({
                    "protocolVersion": message.get("params").and_then(|p| p.get("protocolVersion")).cloned().unwrap_or(serde_json::Value::Null),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "ready", "version": "0.0.0" },
                }),
                Some("tools/list") => serde_json::json!({ "tools": [] }),
                other => return Err(format!("unexpected method {other:?}")),
            };
            Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
    }

    let a = actor_with_mcp(
        vec![stdio("wedged", "true"), stdio("flaky", "true")],
        false,
        vec!["wedged".to_string(), "flaky".to_string()],
    )
    .await;
    a.mcp_strategy.set(McpInitStrategy::Blocking);

    let ready_client = Arc::new(crate::session::mcp_servers::McpClient::new_acp(
        "ready".to_string(),
        "ready-1".to_string(),
        Arc::new(ReadySdkServer),
        None,
        None,
    ));
    ready_client.ensure_initialized().await.expect("handshake");
    assert!(ready_client.is_ready().await);
    a.mcp_state
        .lock()
        .await
        .owned_clients
        .insert("ready".to_string(), ready_client);
    let bridge = a.agent.borrow().tool_bridge().clone();
    register_stub(&bridge, "ready__echo").await;

    let (prepared, took) = timed(prepare(&a, "call-ready", "ready__echo")).await;
    assert!(
        prepared.is_ok(),
        "a ready server's tool must dispatch, got {prepared:?}"
    );
    assert!(
        took < ms(100),
        "ready must not wait on the wedged handshake, took {took:?}"
    );
    assert!(!a.full_mcp_wait_timed_out());

    // Auth-required is recoverable, not terminal: the call must still dispatch so the surfaced auth error can drive re-auth.
    {
        let mut st = a.mcp_state.lock().await;
        st.record_init_failure("ready", true, None);
    }
    let (auth_gated, took) = timed(prepare(&a, "call-auth", "ready__echo")).await;
    assert!(
        auth_gated.is_ok(),
        "an auth-required server's call must dispatch, not reject, got {auth_gated:?}"
    );
    assert!(
        took < ms(100),
        "auth-required must not park the wait, took {took:?}"
    );
    a.mcp_state.lock().await.auth_required.remove("ready");

    let (pending, took) = timed(prepare(&a, "call-wedged", "wedged__do")).await;
    assert!(
        matches!(pending, Err(ToolLoop::NonExistingTool)),
        "a still-connecting server's call rejects rather than waits, got {pending:?}"
    );
    assert!(took < ms(100), "rejecting must not park, took {took:?}");

    {
        let mut st = a.mcp_state.lock().await;
        st.record_init_failure("flaky", false, Some("boom".to_string()));
        st.mark_server_ready("flaky");
    }
    let flaky = prepare(&a, "call-flaky", "flaky__do").await;
    assert!(
        matches!(flaky, Err(ToolLoop::NonExistingTool)),
        "a failed server's call rejects, got {flaky:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_repo_status_prefetch_builds_first_prefix_with_zero_wait() {
    use crate::session::repo_status_prefix::{
        RepoStatusInputs, RepoStatusPlan, RepoStatusPrefetch, RepoStatusPrefetchState,
        RepoStatusSnapshot,
    };
    use distill_workspace::session::git::VcsKind;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;

            let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(None);
            snapshot_tx
                .send(Some(RepoStatusSnapshot {
                    root: None,
                    raw_status: Some("## main\n M src/lib.rs\n".to_string()),
                    vcs_kind: VcsKind::Git,
                }))
                .unwrap();
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::Gather {
                inputs: RepoStatusInputs {
                    cwd: std::path::PathBuf::from("."),
                    vcs_kind: VcsKind::Git,
                    root: None,
                },
                prefetch: std::cell::RefCell::new(Some(RepoStatusPrefetch::from_snapshot_rx(
                    snapshot_rx,
                ))),
            });

            let prefix = actor.build_user_message_prefix().await;

            assert!(
                prefix.contains("<git_status>") && prefix.contains(" M src/lib.rs"),
                "prefix must render the prefetched status, got: {prefix}"
            );
            assert!(
                actor
                    .repo_status_prefetch
                    .take_wait_ms()
                    .is_some_and(|ms| ms < 100),
                "resolved prefetch must not consume the wait budget"
            );
            assert!(
                matches!(
                    actor.repo_status_prefetch.plan(),
                    RepoStatusPlan::Gather { prefetch, .. } if prefetch.borrow().is_none()
                ),
                "prefetch handle is consumed exactly once"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn absent_inputs_omit_the_status_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;

            let prefix = actor.build_user_message_prefix().await;

            assert!(
                !prefix.contains("<git_status>") && !prefix.contains("<jj_status>"),
                "absent inputs must omit the status block, got: {prefix}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resumed_session_gathers_status_inline_without_a_prefetch() {
    use crate::session::repo_status_prefix::{
        RepoStatusInputs, RepoStatusPlan, RepoStatusPrefetchState,
    };
    use distill_workspace::session::git::VcsKind;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            crate::test_support::ensure_hermetic_git_on_path();
            let repo = tempfile::tempdir().unwrap();
            let init = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .expect("spawn git init");
            assert!(init.success(), "git init failed");
            std::fs::write(repo.path().join("untracked.txt"), b"x").unwrap();

            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::Gather {
                inputs: RepoStatusInputs {
                    cwd: repo.path().to_path_buf(),
                    vcs_kind: VcsKind::Git,
                    root: Some(repo.path().to_path_buf()),
                },
                prefetch: std::cell::RefCell::new(None),
            });

            let prefix = actor.build_user_message_prefix().await;

            assert!(
                prefix.contains("<git_status>") && prefix.contains("untracked.txt"),
                "inline gather (no prefetch) must render the status block, got: {prefix}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn suppressed_status_omits_the_body_but_keeps_the_repo_root() {
    use crate::session::repo_status_prefix::{
        RepoStatusPlan, RepoStatusPrefetchState, discover_vcs_root,
    };
    use distill_workspace::session::git::VcsKind;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            crate::test_support::ensure_hermetic_git_on_path();
            let repo = tempfile::tempdir().unwrap();
            let init = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .expect("spawn git init");
            assert!(init.success(), "git init failed");

            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::RootOnly {
                root: discover_vcs_root(repo.path()),
                vcs_kind: VcsKind::Git,
            });

            let prefix = actor.build_user_message_prefix().await;

            assert!(
                !prefix.contains("<git_status>"),
                "suppressed status must omit the body, got: {prefix}"
            );
            assert!(
                matches!(
                    actor.repo_status_prefetch.plan(),
                    RepoStatusPlan::RootOnly { root: Some(_), .. }
                ),
                "suppressed status must keep the discovered repo root"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn replaced_pass_ends_at_once_and_frees_its_server_process() {
    tokio::task::LocalSet::new()
        .run_until(async {
            // A pass whose only server never answers: the spawn lands, the handshake hangs.
            let mut a = actor_with_mcp(vec![], true, vec![]).await;
            let scope = distill_tty_utils::ProcessScope::new();
            a.tool_context.process_scope = Some(scope.clone());
            assert!(
                a.mcp_state
                    .lock()
                    .await
                    .update_configs(vec![stdio("slow", "sleep")])
            );
            a.ensure_mcp_tools_initialized().await;
            tokio::task::yield_now().await;
            assert_eq!(scope.live_count(), 1, "the pass holds its server process");
            assert!(
                a.mcp_state.lock().await.owned_clients.get("slow").is_none(),
                "nothing is handed to the state before its handshake lands"
            );

            assert!(
                a.mcp_state
                    .lock()
                    .await
                    .update_configs(vec![stdio("other", "true")])
            );
            // A pass that waits for its handshake would idle to the 30s startup timeout; only one that ends at the change passes.
            tokio::time::timeout(ms(1), a.mcp_init_tasks.borrow_mut().join_next())
                .await
                .expect("a replaced pass ends at the change, not at its handshake's timeout")
                .expect("the replaced pass ends")
                .expect("the replaced pass ends cleanly");
            assert_eq!(
                scope.live_count(),
                0,
                "a replaced pass frees its server process when it ends"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn startup_outlives_the_caller_that_triggered_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (a, _run_loop) =
                with_run_loop(actor_with_mcp(vec![stdio("slow", "sleep")], false, vec![]).await);
            {
                let init = a.ensure_mcp_tools_initialized();
                tokio::pin!(init);
                assert!(futures::poll!(init.as_mut()).is_pending());
            }
            tokio::task::yield_now().await;
            let st = a.mcp_state.lock().await;
            assert!(
                st.is_initializing() && !st.is_init_abandoned(),
                "startup keeps its owner when the future that triggered it is dropped"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn startup_ends_with_the_run_loop() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (a, run_loop) =
                with_run_loop(actor_with_mcp(vec![stdio("slow", "sleep")], false, vec![]).await);
            let init = a.ensure_mcp_tools_initialized();
            tokio::pin!(init);
            // Polled once: the startup task is queued on the set and has not run.
            assert!(futures::poll!(init.as_mut()).is_pending());
            drop(run_loop);
            init.await;
            assert!(
                !a.mcp_state.lock().await.is_initializing(),
                "a startup task still queued when the run loop ends never runs"
            );
            assert!(
                !a.ensure_mcp_tools_initialized().await
                    && !a.mcp_state.lock().await.is_initializing(),
                "a startup handed over after the run loop ended does not run inline either"
            );
            tokio::time::timeout(ms(1), a.wait_for_mcp_initialized())
                .await
                .expect("a waiter ends when no run loop can start init");
            assert_eq!(
                Arc::strong_count(&a),
                1,
                "no startup task holds the actor once its owner is gone"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn waiters_are_released_only_after_the_final_snapshot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = actor_with_mcp(vec![stdio("fast", "true")], false, vec![]).await;
            a.wait_for_mcp_initialized().await;
            assert!(
                a.tool_metadata_snapshot.lock().unwrap().mcp_initialized,
                "a turn released on completion must already see the completed snapshot"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sign_in_waits_for_the_servers_handshake_to_settle() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = actor_with_mcp(
                vec![stdio("s", "sleep"), stdio("t", "sleep")],
                false,
                vec![],
            )
            .await;
            a.ensure_mcp_tools_initialized().await;
            let settled = a.wait_for_server_settled("s");
            tokio::pin!(settled);
            assert!(
                futures::poll!(settled.as_mut()).is_pending(),
                "while the pass is handshaking the server, no one else touches its slot"
            );
            let change = a
                .update_mcp_configs(&mut *a.mcp_state.lock().await, vec![stdio("s", "sleep")])
                .expect("the set changed");
            assert!(
                futures::poll!(settled.as_mut()).is_pending(),
                "a server-set change hands its successor the claim before the wait can end"
            );
            drop(change);
            tokio::time::timeout(ms(1), settled)
                .await
                .expect("the wait ends once no live pass can hand the server a client");
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn seeding_the_pass_releases_waiters_on_servers_it_leaves_alone() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = actor_with_mcp(
                vec![stdio("a", "sleep"), stdio("b", "sleep")],
                false,
                vec![],
            )
            .await;
            let claim = a.mcp_state.lock().await.restart_init();
            let settled = a.wait_for_server_settled("b");
            tokio::pin!(settled);
            assert!(
                futures::poll!(settled.as_mut()).is_pending(),
                "before the pass seeds its set, any server may still be handed a client"
            );
            a.mcp_state
                .lock()
                .await
                .mark_servers_initializing(["a".to_owned()]);
            tokio::time::timeout(ms(1), settled)
                .await
                .expect("a server the pass leaves alone is released as soon as the pass says so");
            drop(claim);
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn rebuild_pass_yields_to_a_config_change_during_its_relist() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let a = actor_with_mcp(vec![stdio("a", "true")], false, vec![]).await;
            let claim = a.mcp_state.lock().await.restart_init();
            assert!(
                a.mcp_state
                    .lock()
                    .await
                    .update_configs(vec![stdio("b", "true")])
            );
            a.run_mcp_init_with_claim(claim).await;
            assert!(
                !a.mcp_state.lock().await.has_finished_init(),
                "a rebuild whose claim was released by a config change starts no pass of its own"
            );
        })
        .await;
}
