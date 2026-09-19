// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use rmcp::ServiceExt;
use std::path::PathBuf;

#[tokio::test]
async fn resilient_transport_skips_undecodable_line_and_keeps_stream_alive() {
    let (mut server_out, client_in) = tokio::io::duplex(64 * 1024);
    let mut transport = ResilientRwTransport::new(
        client_in,
        tokio::io::sink(),
        "fwbuild".to_string(),
        distill_session_events::EventWriter::noop(),
    );

    let valid = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
    let garbage = "info: fwbuild started, listening on stdio";
    server_out
        .write_all(format!("{valid}\n{garbage}\n{valid}\n").as_bytes())
        .await
        .unwrap();
    drop(server_out);

    assert!(
        transport.receive().await.is_some(),
        "first valid message must be received"
    );
    assert!(
        transport.receive().await.is_some(),
        "the undecodable line must be skipped and the next valid message delivered"
    );
    assert!(
        transport.receive().await.is_none(),
        "only a genuine end-of-stream yields None"
    );
}

fn make_stdio_server(name: &str, command: &str) -> acp::McpServer {
    acp::McpServer::Stdio(acp::McpServerStdio::new(name, PathBuf::from(command)))
}

/// A shared-only session's tools come from inherited clients, so the init waits must not fast-path on "no servers".
#[test]
fn inherited_clients_count_as_mcp_servers() {
    let mut state = McpState::new(vec![]);
    assert!(!state.has_mcp_servers());
    state
        .shared_clients
        .insert("parent".to_string(), Arc::new(McpClient::stub("parent")));
    assert!(state.has_mcp_servers());
}

fn make_http_server(name: &str, url: &str) -> acp::McpServer {
    acp::McpServer::Http(acp::McpServerHttp::new(name, url))
}

#[test]
fn plan_stdio_spawn_windows_resolves_bare_launcher_to_cmd_shim() {
    let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |c| {
        assert_eq!(c, "npx");
        Some(PathBuf::from(r"C:\path\npx.cmd"))
    });
    assert_eq!(program, OsString::from(r"C:\path\npx.cmd"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_unresolved_falls_back_to_raw_command() {
    let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |_| None);
    assert_eq!(program, OsString::from("npx"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_backslash_path_command_used_as_is_without_resolving() {
    let args = vec!["--config".to_string(), "x.json".to_string()];
    let (program, spawn_args) = plan_stdio_spawn(r"C:\tools\server.exe", &args, true, |_| {
        panic!("resolver must not be consulted for a command with a backslash separator")
    });
    assert_eq!(program, OsString::from(r"C:\tools\server.exe"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_forward_slash_path_command_used_as_is_without_resolving() {
    let args = vec!["--port".to_string(), "8080".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("C:/tools/server.exe", &args, true, |_| {
        panic!("resolver must not be consulted for a command with a forward-slash separator")
    });
    assert_eq!(program, OsString::from("C:/tools/server.exe"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_non_windows_never_resolves() {
    let args = vec!["-y".to_string(), "pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, false, |_| {
        panic!("resolver must not be consulted on non-Windows")
    });
    assert_eq!(program, OsString::from("npx"));
    assert_eq!(spawn_args, args);
}

#[test]
fn stdio_path_override_matches_path_case_insensitively() {
    let mk = |name: &str, value: &str| acp::EnvVariable::new(name, value);

    let env = vec![mk("FOO", "bar"), mk("Path", r"C:\node")];
    assert_eq!(stdio_path_override(&env), Some(r"C:\node"));

    let env_upper = vec![mk("PATH", "/custom/bin")];
    assert_eq!(stdio_path_override(&env_upper), Some("/custom/bin"));

    let env_none = vec![mk("FOO", "bar")];
    assert_eq!(stdio_path_override(&env_none), None);
}

#[test]
fn is_figma_mcp_matches_name_and_host() {
    assert!(is_figma_mcp("figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("Figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("grok_com_figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("GROK_COM_FIGMA", "https://example.com/mcp"));
    assert!(is_figma_mcp("grok_com_FIGMA", "https://example.com/mcp"));
    assert!(is_figma_mcp("other", "https://mcp.figma.com/mcp"));
    assert!(is_figma_mcp("other", "https://figma.com/mcp"));
    assert!(!is_figma_mcp("linear", "https://mcp.linear.app/mcp"));
    assert!(!is_figma_mcp("figma_extra", "https://example.com/mcp"));
    assert!(!is_figma_mcp("grok_com_linear", "https://example.com/mcp"));
    assert!(!is_figma_mcp("linear", "not-a-url"));
    assert!(!is_figma_mcp("linear", "https://notfigma.com/mcp"));
    assert!(!is_figma_mcp("linear", "https://figma.com.evil/mcp"));
}

#[test]
fn ensure_figma_user_agent_sets_grok_cli_when_missing() {
    let mut headers = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "grok-cli"
    );

    let mut host_only = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut host_only, "other", "https://mcp.figma.com/mcp");
    assert_eq!(
        host_only.get(reqwest::header::USER_AGENT).unwrap(),
        "grok-cli"
    );
}

#[test]
fn ensure_figma_user_agent_does_not_overwrite_existing() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("custom-ua"),
    );
    ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "custom-ua"
    );
}

#[test]
fn ensure_figma_user_agent_skips_non_figma() {
    let mut headers = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut headers, "linear", "https://mcp.linear.app/mcp");
    assert!(!headers.contains_key(reqwest::header::USER_AGENT));

    let mut invalid_url = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut invalid_url, "linear", "not-a-url");
    assert!(!invalid_url.contains_key(reqwest::header::USER_AGENT));
}

#[test]
fn parse_config_headers_skips_invalid_and_keeps_last_duplicate() {
    let pairs = [
        ("X-Api-Key", "first"),
        ("bad header", "value"),
        ("X-Other", "bad\nvalue"),
        ("x-api-key", "second"),
    ];
    let headers = parse_config_headers("srv", "transport", pairs.iter().copied());
    assert_eq!(headers.len(), 1);
    assert_eq!(headers.get("X-Api-Key").unwrap(), "second");
}

#[test]
fn apply_user_agent_policy_sets_versioned_grok_cli() {
    let mut headers = reqwest::header::HeaderMap::new();
    apply_user_agent_policy(&mut headers, "linear", "https://mcp.linear.app/mcp");
    let expected = format!("grok-cli/{}", distill_version::VERSION);
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        expected.as_str()
    );
}

#[test]
fn apply_user_agent_policy_preserves_configured_user_agent() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("custom-ua"),
    );
    apply_user_agent_policy(&mut headers, "linear", "https://mcp.linear.app/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "custom-ua"
    );
}

#[test]
fn apply_user_agent_policy_preserves_figma_attribution() {
    let mut headers = reqwest::header::HeaderMap::new();
    apply_user_agent_policy(&mut headers, "other", "https://mcp.figma.com/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "grok-cli"
    );
}

#[cfg(unix)]
#[test]
fn safe_stdio_child_drop_without_entered_runtime_reaps_child() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (transport, pid) = rt.block_on(async {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").kill_on_drop(true);
        distill_tools::util::detach_command(&mut cmd);
        let (transport, _stderr) = SafeTokioChildProcess::spawn(
            cmd,
            None,
            "test".to_string(),
            distill_session_events::EventWriter::noop(),
        )
        .await
        .expect("spawn test child");
        let pid = transport.id().expect("spawned child pid");
        (transport, pid)
    });

    drop(rt);
    drop(transport);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !unix_process_exists(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    panic!("MCP child process {pid} was not reaped after no-runtime drop");
}

#[cfg(unix)]
fn unix_process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
#[tokio::test]
async fn scope_kill_all_reaps_enrolled_mcp_child_while_owner_wedged() {
    use std::time::Duration;

    let scope = ProcessScope::new();

    let mut cmd = Command::new("sleep");
    cmd.arg("600").kill_on_drop(true);
    distill_tools::util::detach_command(&mut cmd);
    let (mut child_process, _stderr) = SafeTokioChildProcess::spawn(
        cmd,
        Some(&scope),
        "wedge-test".to_string(),
        distill_session_events::EventWriter::noop(),
    )
    .await
    .expect("spawn enrolled MCP child");
    assert_eq!(
        scope.live_count(),
        1,
        "the enrolled MCP child group must be tracked by the scope"
    );

    scope.kill_all();

    let mut child = child_process.child.take().expect("child handle present");
    child_process.process_group = None;
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("scope.kill_all must have SIGKILL'd the enrolled MCP child group")
        .expect("wait on the reclaimed child succeeds");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the MCP child must have been SIGKILL'd by the scope, not have exited cleanly"
    );
}

#[test]
fn test_mcp_state_new() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let state = McpState::new(configs.clone());

    assert_eq!(state.configs.len(), 1);
    assert!(state.owned_clients.is_empty());
    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
}

#[test]
fn config_update_clears_stale_failure_records() {
    let mut state = McpState::new(vec![make_http_server("a", "https://old.example/a")]);
    state.record_init_failure("a", false, Some("old cause".to_string()));
    let diff = state
        .update_configs_diff(vec![make_http_server("a", "https://new.example/a")])
        .expect("configs changed");
    assert_eq!(diff.removed, vec!["a".to_string()]);
    assert!(
        !state.init_failed.contains_key("a"),
        "changed config must clear the stale failure record"
    );

    let mut state = McpState::new(vec![make_http_server("b", "https://old.example/b")]);
    state.record_init_failure("b", false, Some("old cause".to_string()));
    assert!(state.update_configs(vec![make_http_server("b", "https://new.example/b")]));
    assert!(state.init_failed.is_empty());
}

#[test]
fn test_mcp_state_update_configs_returns_false_when_unchanged() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs.clone());
    let generation = state.current_generation();

    let changed = state.update_configs(configs.clone());
    assert!(!changed);
    assert!(
        !generation.is_cancelled(),
        "an unchanged set keeps its generation"
    );
}

#[test]
fn test_mcp_state_update_configs_returns_true_when_changed() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs);
    let generation = state.current_generation();

    let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
    let changed = state.update_configs(new_configs);
    assert!(changed);
    assert!(
        generation.is_cancelled(),
        "a changed set cancels its generation"
    );
    assert!(!state.current_generation().is_cancelled());
}

#[test]
fn test_mcp_state_update_configs_resets_initialized() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs);
    let _owner = state.try_start_init().expect("claims init");
    state.mark_servers_initializing(["a".to_string()]);
    state.finish_init();
    assert!(state.has_finished_init());
    assert!(state.is_server_handshaking("a"));

    let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
    let changed = state.update_configs(new_configs);
    assert!(changed);
    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
}

#[tokio::test]
async fn acp_servers_survive_update_configs_clear() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let mut state = McpState::new(vec![make_http_server("http-srv", "http://localhost")]);
    state.set_acp_servers(
        vec![AcpServerEntry {
            name: "sdk-tools".to_string(),
            server_id: "srv_0".to_string(),
        }],
        Arc::new(NoopInvoker),
    );
    assert!(state.has_acp_servers());
    assert_eq!(state.build_pending_acp_clients(&HashMap::new()).len(), 1);

    let changed = state.update_configs(vec![make_http_server("other", "http://other")]);
    assert!(changed);
    assert!(
        state.has_acp_servers(),
        "acp servers must survive update_configs"
    );
    let pending = state.build_pending_acp_clients(&HashMap::new());
    assert_eq!(pending.len(), 1, "acp clients rebuild after the clear");
    assert_eq!(pending.first().map(|c| c.server_name()), Some("sdk-tools"));
}

#[tokio::test]
async fn acp_overrides_apply_to_built_clients() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let mut overrides = HashMap::new();
    overrides.insert(
        "sdk-tools".to_string(),
        McpClientTimeoutOverrides {
            tool_timeout_sec: Some(123),
            ..Default::default()
        },
    );

    let mut state = McpState::new(vec![]);
    state.set_acp_servers(
        vec![AcpServerEntry {
            name: "sdk-tools".to_string(),
            server_id: "srv_0".to_string(),
        }],
        Arc::new(NoopInvoker),
    );

    let pending = state.build_pending_acp_clients(&overrides);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending.first().map(|c| c.tool_timeout_sec()),
        Some(123),
        "config.toml tool_timeout_sec override must reach the SDK client"
    );
}

#[tokio::test]
async fn acp_clients_are_not_liveness_watched() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let acp = McpClient::new_acp(
        "sdk".to_string(),
        "srv_0".to_string(),
        Arc::new(NoopInvoker),
        None,
        None,
    );
    assert!(acp.is_acp());
    assert!(!acp.is_http());

    let http = McpClient::new_http(
        "http".to_string(),
        HttpConfig {
            url: "http://localhost/api/mcp".to_string(),
            headers: vec![],
            local_agent_endpoint: false,
        },
        None,
        None,
    );
    assert!(!http.is_acp());

    assert!(!McpClient::stub("stdio").is_acp());

    assert!(
        !Arc::new(acp)
            .arm_liveness_watcher(Duration::from_millis(500))
            .await
    );
}

#[test]
fn test_mark_servers_initializing_clears_prior_init_failure() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    state.init_failed.insert("a".to_string(), String::new());
    state.init_failed.insert("b".to_string(), String::new());

    state.mark_servers_initializing(["a".to_string()]);

    assert!(
        !state.init_failed.contains_key("a"),
        "fresh init attempt must clear the prior failure for that server",
    );
    assert!(
        state.init_failed.contains_key("b"),
        "servers not in this init attempt must keep their failure flag",
    );
}

#[test]
fn test_record_init_failure_keeps_auth_and_init_failed_disjoint() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    state.record_init_failure("auth-srv", true, None);
    assert!(state.auth_required.contains("auth-srv"));
    assert!(
        !state.init_failed.contains_key("auth-srv"),
        "auth-required failures must not also be flagged init_failed",
    );

    state.record_init_failure(
        "dead-srv",
        false,
        Some("tools/list failed: boom".to_string()),
    );
    assert!(!state.auth_required.contains("dead-srv"));
    assert_eq!(
        state.init_failed.get("dead-srv").map(String::as_str),
        Some("tools/list failed: boom"),
    );

    state.mark_servers_initializing(["dead-srv".to_string()]);
    assert!(!state.init_failed.contains_key("dead-srv"));
}

#[test]
fn test_clear_init_failed_removes_entry() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    state.record_init_failure("dead-srv", false, Some("boom".to_string()));
    assert!(state.init_failed.contains_key("dead-srv"));

    state.clear_init_failed("dead-srv");
    assert!(!state.init_failed.contains_key("dead-srv"));
    state.clear_init_failed("never-seen");
}

#[test]
fn test_mcp_servers_equal_empty_lists() {
    let a: Vec<acp::McpServer> = vec![];
    let b: Vec<acp::McpServer> = vec![];
    assert!(mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_identical_configs() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![make_stdio_server("test", "/bin/test")];
    assert!(mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_names() {
    let a = vec![make_stdio_server("test1", "/bin/test")];
    let b = vec![make_stdio_server("test2", "/bin/test")];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_lengths() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![
        make_stdio_server("test", "/bin/test"),
        make_stdio_server("test2", "/bin/test2"),
    ];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_types() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![make_http_server("test", "http://localhost")];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_order_matters() {
    let a = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let b = vec![
        make_stdio_server("b", "/bin/b"),
        make_stdio_server("a", "/bin/a"),
    ];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_try_start_init_prevents_concurrent_init() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

    let _owner = state.try_start_init().expect("claims init");
    assert!(state.is_initializing());
    assert!(!state.is_initialized());

    assert!(state.try_start_init().is_none());
}

#[test]
fn test_try_start_init_fails_when_initialized() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
    let _owner = state.try_start_init().expect("claims init");
    state.finish_init();
    state.complete_init();
    assert!(state.is_initialized());

    assert!(state.try_start_init().is_none());
    assert!(!state.is_initializing());
    assert!(state.is_initialized(), "is_initialized stays true");
}

#[test]
fn test_finish_init_clears_initializing() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

    let _owner = state.try_start_init().expect("claims init");
    assert!(state.is_initializing());
    assert!(!state.is_initialized());

    state.finish_init();
    assert!(
        state.is_initializing(),
        "finish_init alone is not completion"
    );
    state.complete_init();
    assert!(!state.is_initializing());
    assert!(state.is_initialized());
}

#[test]
fn test_update_configs_resets_initializing() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
    let _owner = state.try_start_init().expect("claims init");
    assert!(state.is_initializing());

    state.update_configs(vec![make_stdio_server("test2", "/bin/test2")]);
    assert!(!state.is_initializing());
    assert!(!state.is_initialized());
}

#[test]
fn test_parse_mcp_meta_config_with_tool_timeouts_ms() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "github": {
                "toolTimeoutMs": 60000,
                "toolTimeoutsMs": {
                    "create_issue": 120000,
                    "search": 30000
                }
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    let github = map.get("github").unwrap();
    assert_eq!(github.tool_timeout_ms, Some(60000));
    let tt = github.tool_timeouts_ms.as_ref().unwrap();
    assert_eq!(tt.get("create_issue"), Some(&120000));
    assert_eq!(tt.get("search"), Some(&30000));
}

#[test]
fn test_parse_mcp_meta_config_without_tool_timeouts_ms() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "github": {
                "toolTimeoutMs": 60000
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    let github = map.get("github").unwrap();
    assert_eq!(github.tool_timeout_ms, Some(60000));
    assert!(github.tool_timeouts_ms.is_none());
    assert!(github.expose_image_base64.is_none());
}

#[test]
fn test_parse_mcp_meta_config_with_expose_image_base64() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "grafana": { "exposeImageBase64": true },
            "linear":  { "exposeImageBase64": false },
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    assert_eq!(map.get("grafana").unwrap().expose_image_base64, Some(true));
    assert_eq!(map.get("linear").unwrap().expose_image_base64, Some(false));
}

#[test]
fn test_tool_timeout_for_returns_per_tool_override() {
    let mut tool_timeouts = HashMap::new();
    tool_timeouts.insert("create_issue".to_string(), 120u64);
    tool_timeouts.insert("search".to_string(), 30u64);

    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        tool_timeout_sec: Some(60),
        tool_timeouts: Some(tool_timeouts),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "github".to_string(),
        HttpConfig {
            url: String::new(),
            headers: vec![],
            local_agent_endpoint: false,
        },
        Some(&overrides),
        None,
    );

    assert_eq!(client.tool_timeout_for("create_issue"), 120);
    assert_eq!(client.tool_timeout_for("search"), 30);
    assert_eq!(client.tool_timeout_for("list_repos"), 60);
    assert_eq!(client.tool_timeout_for(""), 60);
}

#[test]
fn test_tool_timeout_for_empty_map_returns_default() {
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        tool_timeout_sec: Some(45),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "test".to_string(),
        HttpConfig {
            url: String::new(),
            headers: vec![],
            local_agent_endpoint: false,
        },
        Some(&overrides),
        None,
    );

    assert_eq!(client.tool_timeout_for("any_tool"), 45);
    assert_eq!(client.tool_timeout_sec(), 45);
}

#[test]
fn test_load_timeouts_startup_precedence() {
    assert_eq!(
        McpClient::load_timeouts(None, None).0,
        DEFAULT_STARTUP_TIMEOUT_SECS
    );

    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(7),
        ..Default::default()
    };
    assert_eq!(McpClient::load_timeouts(Some(&overrides), None).0, 7);

    let meta = McpServerMetaConfig {
        startup_timeout_ms: Some(12_000),
        ..Default::default()
    };
    assert_eq!(
        McpClient::load_timeouts(Some(&overrides), Some(&meta)).0,
        12
    );
}

#[test]
fn test_update_configs_diff_no_change() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs.clone());
    let generation = state.current_generation();
    assert!(state.update_configs_diff(configs).is_none());
    assert!(!generation.is_cancelled());
}

#[test]
fn test_update_configs_diff_added() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);
    let generation = state.current_generation();

    let new_configs = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["a"]);
    assert_eq!(diff.added, vec!["b"]);
    assert!(diff.removed.is_empty());
    assert!(generation.is_cancelled());
}

#[test]
fn test_update_configs_diff_removed() {
    let configs = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let mut state = McpState::new(configs);

    let new_configs = vec![make_stdio_server("a", "/bin/a")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["a"]);
    assert!(diff.added.is_empty());
    assert_eq!(diff.removed, vec!["b"]);
}

#[test]
fn test_update_configs_diff_changed() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);

    let new_configs = vec![make_stdio_server("a", "/bin/a_v2")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert_eq!(diff.added, vec!["a"]);
    assert_eq!(diff.removed, vec!["a"]);
}

#[test]
fn test_update_configs_diff_auth_required_cleanup() {
    let configs = vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("remove", "/bin/remove"),
    ];
    let mut state = McpState::new(configs);
    state.auth_required.insert("remove".to_string());
    state.auth_required.insert("keep".to_string());

    let new_configs = vec![make_stdio_server("keep", "/bin/keep")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["keep"]);
    assert_eq!(diff.removed, vec!["remove"]);
    assert!(state.auth_required.contains("keep"));
    assert!(!state.auth_required.contains("remove"));
}

#[test]
fn test_update_configs_diff_empty_to_nonempty() {
    let mut state = McpState::new(vec![]);
    let new_configs = vec![make_stdio_server("a", "/bin/a")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert_eq!(diff.added, vec!["a"]);
    assert!(diff.removed.is_empty());
}

#[test]
fn test_update_configs_diff_nonempty_to_empty() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);
    let diff = state
        .update_configs_diff(vec![])
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert!(diff.added.is_empty());
    assert_eq!(diff.removed, vec!["a"]);
}

#[test]
fn test_mcp_erased_tool_id_is_qualified() {
    use distill_tool_runtime::Tool;

    let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));

    let tool_a = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users".to_string(),
            "calendar".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };
    let tool_b = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users".to_string(),
            "teams".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let id_a = tool_a.id();
    let id_b = tool_b.id();

    assert_eq!(id_a.as_str(), "calendar__SearchUsers");
    assert_eq!(id_b.as_str(), "teams__SearchUsers");

    assert_ne!(id_a, id_b);
}

#[test]
fn test_same_raw_name_different_servers_no_local_registry_collision() {
    use distill_computer_hub_sdk::LocalRegistry;
    use distill_tool_runtime::Tool;

    let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));
    let registry = LocalRegistry::new();

    let tool_a = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users on calendar".to_string(),
            "calendar".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };
    let tool_b = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users on teams".to_string(),
            "teams".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let id_a = tool_a.id();
    let id_b = tool_b.id();

    let displaced_a = registry.register(tool_a);
    assert!(
        displaced_a.is_none(),
        "first registration should not displace"
    );

    let displaced_b = registry.register(tool_b);
    assert!(
        displaced_b.is_none(),
        "second registration must not overwrite first"
    );

    assert!(
        registry.find(&id_a).is_some(),
        "calendar tool must be found"
    );
    assert!(registry.find(&id_b).is_some(), "teams tool must be found");
    assert_eq!(registry.len(), 2);
}

fn make_test_client(name: &str) -> Arc<McpClient> {
    Arc::new(McpClient::stub(name))
}

#[test]
fn test_shared_mcp_pool_from_empty_state() {
    let state = McpState::new(vec![]);
    let pool = SharedMcpPool::from_state(&state);
    assert_eq!(pool.len(), 0);
    assert_eq!(pool.server_names().count(), 0);
    assert!(pool.configs().is_empty());
    assert!(pool.meta_config_map().is_empty());
    assert!(pool.get_client("anything").is_none());
}

#[test]
fn test_shared_mcp_pool_len_matches_client_count() {
    let mut state = McpState::new(vec![]);
    for name in ["alpha", "beta", "gamma"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let pool = SharedMcpPool::from_state(&state);
    assert_eq!(pool.len(), 3);
    assert_eq!(pool.len(), pool.server_names().count());
}

#[test]
fn test_shared_mcp_pool_snapshot_shares_arc_clients() {
    let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
    let client = make_test_client("github");
    state
        .owned_clients
        .insert("github".to_string(), Arc::clone(&client));

    let pool = SharedMcpPool::from_state(&state);
    let pool_client = pool.get_client("github").expect("should find client");

    assert!(Arc::ptr_eq(&client, pool_client));
}

#[test]
fn test_shared_mcp_pool_get_client_missing() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("a".to_string(), make_test_client("a"));
    let pool = SharedMcpPool::from_state(&state);

    assert!(pool.get_client("a").is_some());
    assert!(pool.get_client("nonexistent").is_none());
    assert!(pool.get_client("").is_none());
}

#[test]
fn test_shared_mcp_pool_server_names() {
    let mut state = McpState::new(vec![]);
    for name in ["alpha", "beta", "gamma"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }

    let pool = SharedMcpPool::from_state(&state);
    let mut names: Vec<&str> = pool.server_names().collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
}

#[test]
fn test_shared_mcp_pool_snapshot_independent_of_state_mutations() {
    let mut state = McpState::new(vec![make_stdio_server("srv", "/bin/srv")]);
    state
        .owned_clients
        .insert("srv".to_string(), make_test_client("srv"));

    let pool = SharedMcpPool::from_state(&state);

    state.owned_clients.clear();
    state.configs.clear();

    assert_eq!(pool.server_names().count(), 1);
    assert!(pool.get_client("srv").is_some());
    assert_eq!(pool.configs().len(), 1);
}

#[test]
fn test_shared_mcp_pool_meta_config_preserved() {
    let mut meta = McpMetaConfigMap::new();
    meta.insert(
        "github".to_string(),
        McpServerMetaConfig {
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(120000),
            tool_timeouts_ms: None,
            expose_image_base64: None,
        },
    );
    let state = McpState::new_with_meta(vec![make_http_server("github", "http://gh.local")], meta);
    let pool = SharedMcpPool::from_state(&state);

    let mc = pool
        .meta_config_map()
        .get("github")
        .expect("should have meta config");
    assert_eq!(mc.startup_timeout_ms, Some(5000));
    assert_eq!(mc.tool_timeout_ms, Some(120000));
}

#[test]
fn test_get_client_owned_overrides_shared() {
    let mut state = McpState::new(vec![]);
    let shared = make_test_client("srv");
    let owned = make_test_client("srv");
    state
        .shared_clients
        .insert("srv".to_string(), Arc::clone(&shared));
    state
        .owned_clients
        .insert("srv".to_string(), Arc::clone(&owned));

    let got = state.get_client("srv").unwrap();
    assert!(Arc::ptr_eq(got, &owned));
    assert!(!Arc::ptr_eq(got, &shared));
}

#[test]
fn test_get_client_falls_through_to_shared() {
    let mut state = McpState::new(vec![]);
    let shared = make_test_client("srv");
    state
        .shared_clients
        .insert("srv".to_string(), Arc::clone(&shared));

    let got = state.get_client("srv").unwrap();
    assert!(Arc::ptr_eq(got, &shared));
    assert!(state.get_client("missing").is_none());
}

#[test]
fn test_all_clients_deduplicates_shared_by_owned() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("a".to_string(), make_test_client("a"));
    state
        .shared_clients
        .insert("a".to_string(), make_test_client("a-shared"));
    state
        .shared_clients
        .insert("b".to_string(), make_test_client("b-shared"));

    let all: Vec<_> = state.all_clients().map(|(n, _)| n.as_str()).collect();
    assert_eq!(all.iter().filter(|&&n| n == "a").count(), 1);
    assert!(all.contains(&"b"));
    assert_eq!(all.len(), 2);

    let (_, a_client) = state.all_clients().find(|(n, _)| *n == "a").unwrap();
    assert!(Arc::ptr_eq(a_client, state.owned_clients.get("a").unwrap()));
}

#[test]
fn test_import_shared_clients_skips_config_collisions() {
    let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
    let mut pool_clients = HashMap::new();
    pool_clients.insert("github".to_string(), make_test_client("github"));
    pool_clients.insert("linear".to_string(), make_test_client("linear"));
    let pool = SharedMcpPool {
        clients: pool_clients,
        configs: vec![],
        meta_config_map: McpMetaConfigMap::new(),
    };

    state.import_shared_clients(&pool);

    assert!(
        !state.shared_clients.contains_key("github"),
        "github should be skipped — collides with child config"
    );
    assert!(
        state.shared_clients.contains_key("linear"),
        "linear should be imported — no collision"
    );
}

#[test]
fn test_update_configs_preserves_shared_clients() {
    let mut state = McpState::new(vec![make_stdio_server("old", "/bin/old")]);
    state
        .owned_clients
        .insert("old".to_string(), make_test_client("old"));
    let shared = make_test_client("inherited");
    state
        .shared_clients
        .insert("inherited".to_string(), Arc::clone(&shared));

    let changed = state.update_configs(vec![make_stdio_server("new", "/bin/new")]);

    assert!(changed);
    assert!(state.owned_clients.is_empty(), "owned should be cleared");
    assert_eq!(state.shared_clients.len(), 1, "shared should be untouched");
    assert!(Arc::ptr_eq(
        state.shared_clients.get("inherited").unwrap(),
        &shared
    ));
}

#[test]
fn test_update_configs_diff_preserves_shared_clients() {
    let mut state = McpState::new(vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("drop", "/bin/drop"),
    ]);
    state
        .owned_clients
        .insert("keep".to_string(), make_test_client("keep"));
    state
        .owned_clients
        .insert("drop".to_string(), make_test_client("drop"));
    let shared = make_test_client("inherited");
    state
        .shared_clients
        .insert("inherited".to_string(), Arc::clone(&shared));

    let diff = state
        .update_configs_diff(vec![make_stdio_server("keep", "/bin/keep")])
        .expect("configs changed");

    assert!(diff.removed.contains(&"drop".to_string()));
    assert!(diff.retained.contains(&"keep".to_string()));
    assert!(!state.owned_clients.contains_key("drop"));
    assert!(state.owned_clients.contains_key("keep"));
    assert!(Arc::ptr_eq(
        state.shared_clients.get("inherited").unwrap(),
        &shared
    ));
}

#[test]
fn test_from_state_captures_both_owned_and_shared() {
    let mut state = McpState::new(vec![]);
    let owned = make_test_client("owned-srv");
    let shared = make_test_client("shared-srv");
    state
        .owned_clients
        .insert("owned-srv".to_string(), Arc::clone(&owned));
    state
        .shared_clients
        .insert("shared-srv".to_string(), Arc::clone(&shared));

    let pool = SharedMcpPool::from_state(&state);

    assert!(Arc::ptr_eq(pool.get_client("owned-srv").unwrap(), &owned));
    assert!(Arc::ptr_eq(pool.get_client("shared-srv").unwrap(), &shared));
    assert_eq!(pool.server_names().count(), 2);
}

#[test]
fn test_retain_clients_keeps_matching() {
    let mut state = McpState::new(vec![]);
    for name in ["github", "linear", "slack"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|name| name == "github" || name == "slack");

    assert!(pool.get_client("github").is_some());
    assert!(pool.get_client("slack").is_some());
    assert!(pool.get_client("linear").is_none());
    assert_eq!(pool.server_names().count(), 2);
}

#[test]
fn test_retain_clients_remove_all() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("srv".to_string(), make_test_client("srv"));
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|_| false);

    assert_eq!(pool.server_names().count(), 0);
    assert!(pool.get_client("srv").is_none());
}

#[test]
fn test_retain_clients_keep_all() {
    let mut state = McpState::new(vec![]);
    for name in ["a", "b", "c"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|_| true);

    assert_eq!(pool.server_names().count(), 3);
}

#[test]
fn test_retain_clients_preserves_arc_identity() {
    let mut state = McpState::new(vec![]);
    let client = make_test_client("keep");
    state
        .owned_clients
        .insert("keep".to_string(), Arc::clone(&client));
    state
        .owned_clients
        .insert("drop".to_string(), make_test_client("drop"));
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|name| name == "keep");

    assert!(Arc::ptr_eq(pool.get_client("keep").unwrap(), &client));
}

fn make_mcp_tool(server_name: &str, name: &str) -> McpTool {
    McpTool::new(
        name.to_string(),
        "test desc".to_string(),
        server_name.to_string(),
        Arc::new(Mutex::new(McpState::new(vec![]))),
        serde_json::json!({}),
        None,
    )
}

#[test]
fn into_registration_validates_qualified_name() {
    let registration = make_mcp_tool("linear", "list_issues")
        .into_registration()
        .expect("should register");
    assert_eq!(registration.name, "linear__list_issues");

    let digit_tool = make_mcp_tool("auth", "2fa_enable")
        .into_registration()
        .expect("digit-leading tool segments are catalog-valid");
    assert_eq!(digit_tool.name, "auth__2fa_enable");

    for (server, tool) in [
        ("server__part", "tool"),
        ("server", "tool__part"),
        ("foo_", "bar"),
        ("foo", "_bar"),
        ("foo_", "_bar"),
        ("", "tool"),
        ("server", ""),
        ("123", "lookup"),
        ("server:scope", "tool"),
    ] {
        assert!(
            make_mcp_tool(server, tool).into_registration().is_err(),
            "unexpectedly registered {server:?} and {tool:?}"
        );
    }
}

#[test]
fn into_registration_admits_qualified_names_longer_than_provider_64() {
    let server_62 = format!("a{}", "b".repeat(61));
    let qualified = format!("{server_62}__b");
    assert_eq!(qualified.len(), 65);
    assert!(parse_mcp_qualified_name(&qualified).is_some());
    assert!(validate_tool_name(&qualified).is_err());
    let registration = make_mcp_tool(&server_62, "b")
        .into_registration()
        .expect("qualified catalog keys may exceed the 64-char provider budget");
    assert_eq!(registration.name, qualified);
}

#[test]
fn test_is_retriable_transport_closed() {
    assert!(is_retriable_transport_error(&ServiceError::TransportClosed));
}

#[test]
fn test_is_retriable_transport_send() {
    let err = ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
        "test",
        std::any::TypeId::of::<()>(),
        Box::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "connection reset",
        )),
    ));
    assert!(is_retriable_transport_error(&err));
}

#[test]
fn test_not_retriable_unexpected_response() {
    assert!(!is_retriable_transport_error(
        &ServiceError::UnexpectedResponse
    ));
}

#[test]
fn test_not_retriable_cancelled() {
    assert!(!is_retriable_transport_error(&ServiceError::Cancelled {
        reason: Some("shutdown".to_string()),
    }));
}

#[test]
fn test_not_retriable_timeout() {
    assert!(!is_retriable_transport_error(&ServiceError::Timeout {
        timeout: std::time::Duration::from_secs(30),
    }));
}

fn mcp_service_err(code: i32) -> ServiceError {
    ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(code),
        "boom",
        None,
    ))
}

#[test]
fn should_recover_mcp_error_recovers_everything_outside_excluded_set() {
    assert!(should_recover_mcp_error(-32603));
    assert!(should_recover_mcp_error(-32002));
    assert!(should_recover_mcp_error(-32000));
    assert!(should_recover_mcp_error(-32099));
    assert!(should_recover_mcp_error(-32100));
    assert!(should_recover_mcp_error(0));
    assert!(should_recover_mcp_error(1));
    assert!(should_recover_mcp_error(i32::MIN));
    assert!(should_recover_mcp_error(i32::MAX));
}

#[test]
fn should_recover_mcp_error_skips_deterministic_client_errors() {
    assert!(!should_recover_mcp_error(-32700));
    assert!(!should_recover_mcp_error(-32600));
    assert!(!should_recover_mcp_error(-32601));
    assert!(!should_recover_mcp_error(-32602));
}

#[test]
fn should_recover_service_error_http_mcperror_recoverable() {
    assert!(should_recover_service_error(
        &mcp_service_err(-32603),
        true,
        false,
    ));
}

#[test]
fn should_recover_service_error_http_mcperror_invalid_params_skipped() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32602),
        true,
        false,
    ));
}

#[test]
fn should_recover_service_error_stdio_mcperror_not_recovered() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32603),
        false,
        false,
    ));
}

#[test]
fn should_recover_service_error_mcperror_at_most_once_per_dispatch() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32603),
        true,
        true,
    ));
}

#[test]
fn should_recover_service_error_http_mcperror_auth_rejection_not_recovered() {
    let auth_err = ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32603),
        "Unauthorized: token expired",
        None,
    ));
    assert!(!should_recover_service_error(&auth_err, true, false));
    let session_err = ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32603),
        "session not found",
        None,
    ));
    assert!(should_recover_service_error(&session_err, true, false));
}

#[test]
fn should_recover_service_error_transport_errors_always_recover() {
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        true,
        false
    ));
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        false,
        false
    ));
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        true,
        true
    ));
}

#[test]
fn should_recover_service_error_other_non_transport_not_recovered() {
    assert!(!should_recover_service_error(
        &ServiceError::UnexpectedResponse,
        true,
        false
    ));
    assert!(!should_recover_service_error(
        &ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(30),
        },
        true,
        false
    ));
}

#[tokio::test]
async fn recover_and_retry_surfaces_original_error_when_recover_fails() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "wedged".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let tool = McpErasedTool {
        tool: McpTool::new(
            "do_thing".to_string(),
            "desc".to_string(),
            "wedged".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let original = mcp_service_err(-32603);
    let expected = original.to_string();
    let params = CallToolRequestParams::new("do_thing");

    let mut reconnect_attempted = false;
    let mut is_timeout = false;
    let ew = distill_session_events::EventWriter::noop();

    let err = tool
        .recover_and_retry(
            &client,
            params,
            std::time::Duration::from_secs(1),
            1,
            original,
            &mut reconnect_attempted,
            &mut is_timeout,
            &ew,
        )
        .await
        .expect_err("recover must fail against an unreachable host");

    assert_eq!(err.to_string(), expected, "original error must be surfaced");
    assert!(reconnect_attempted, "reconnect attempt must be flagged");
    assert!(!is_timeout, "a recover failure is not a tool timeout");
}

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum CallToolBehavior {
    ErrorThenOk {
        code: i32,
    },
    AlwaysError {
        code: i32,
    },
    HangThenOk {
        hang_ms: u64,
    },
    ErrorThenHang {
        code: i32,
        hang_ms: u64,
    },
    /// SEP-2322: first `tools/call` returns `input_required` with a form
    /// elicitation and a `requestState`; the retry completes.
    FormElicitThenOk,
    /// SEP-2322 + transport recovery: round 0 returns `input_required`, the
    /// retry fails with a recoverable JSON-RPC error, and the post-recovery
    /// retry completes.
    FormElicitThenErrorThenOk,
    /// SEP-2322 URL mode: round 0 returns a URL elicitation + `requestState`,
    /// round 1 returns a `requestState`-only `input_required` (out-of-band
    /// interaction still pending), round 2 completes.
    UrlElicitThenStateOnlyThenOk,
    /// Structured-first: summary in `content`, payload in `structuredContent`.
    StructuredOk,
    /// A stateless 2026-07-28 server (C# SDK default): answers `initialize`, but each POST is a
    /// fresh instance, so `tools/call` fails with `code` unless `_meta` carries the elicitation
    /// capability. Real servers send -32021 (missing capability) or -32022 (unsupported version).
    RequiresPerRequestCapabilities {
        code: i32,
    },
}

#[derive(Clone)]
struct FakeMcpHandles {
    inits: Arc<AtomicUsize>,
    discovers: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    init_version: Arc<parking_lot::Mutex<Option<String>>>,
    init_user_agents: Arc<parking_lot::Mutex<Vec<String>>>,
    /// `capabilities` object from the last `initialize` request.
    init_capabilities: Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
    /// Raw `tools/call` request bodies, for asserting MRTR retry wire shapes.
    call_bodies: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    /// `MCP-Protocol-Version` header of each `tools/call`, parallel to `call_bodies`.
    call_version_headers: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
    /// When set, replaces `FakeMcpOptions::discover` for later probes, so a test can turn a
    /// legacy server modern between a handshake and a recovery.
    discover_override: Arc<parking_lot::Mutex<Option<DiscoverBehavior>>>,
}

fn header_values(
    headers: &axum::http::HeaderMap,
    name: axum::http::header::HeaderName,
) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect()
}

/// How the fake reacts to the SEP-2575 `server/discover` probe.
#[derive(Clone, Copy, Default)]
enum DiscoverBehavior {
    /// JSON-RPC method-not-found — the typical legacy SDK reaction.
    #[default]
    MethodNotFound,
    /// A modern server: discover succeeds with 2026-07-28.
    Modern,
    /// Legacy middleware that 500s the probe with a non-JSON body — one of the
    /// malformed shapes that must still fall back to `initialize`.
    NonJsonServerError,
}

#[derive(Clone, Default)]
struct FakeMcpOptions {
    discover: DiscoverBehavior,
    /// When set, `initialize` is rejected with an "Unauthorized" JSON-RPC
    /// error, for asserting that fallback errors keep their auth classification.
    init_unauthorized: bool,
    /// When set, `tools/list` never answers: the server connects, then stalls.
    list_tools_hangs: bool,
}

#[derive(Clone)]
struct FakeMcpState {
    behavior: CallToolBehavior,
    options: FakeMcpOptions,
    handles: FakeMcpHandles,
}

async fn fake_handle_post(
    axum::extract::State(state): axum::extract::State<FakeMcpState>,
    headers: axum::http::HeaderMap,
    axum::Json(req): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let params = req.get("params");
    let ok = || {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {"content": [{"type": "text", "text": "ok"}], "isError": false},
        })
    };
    let err = |code: i32, msg: String| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "error": {"code": code, "message": msg},
        })
    };
    match req.get("method").and_then(|m| m.as_str()) {
        Some("initialize") => {
            state.handles.inits.fetch_add(1, Ordering::Relaxed);
            *state.handles.init_version.lock() = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            *state.handles.init_capabilities.lock() =
                params.and_then(|p| p.get("capabilities")).cloned();
            state
                .handles
                .init_user_agents
                .lock()
                .extend(header_values(&headers, axum::http::header::USER_AGENT));
            let requested = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // Version-strict servers (e.g. the C# SDK 2.x) reject an `initialize`
            // naming a post-handshake revision instead of counter-offering.
            // Mimic that so a reintroduced 2026-07-28 pin fails every test here.
            if requested == "2026-07-28" {
                return axum::Json(err(
                    -32602,
                    format!(
                        "Protocol version '{requested}' is not available through the initialize handshake."
                    ),
                ))
                .into_response();
            }
            if state.options.init_unauthorized {
                return axum::Json(err(-32001, "Unauthorized: token expired".to_string()))
                    .into_response();
            }
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "result": {
                    "protocolVersion": params
                        .and_then(|p| p.get("protocolVersion"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "0.0.0"},
                },
            });
            ([("mcp-session-id", "fake-session")], axum::Json(result)).into_response()
        }
        Some("server/discover") => {
            state.handles.discovers.fetch_add(1, Ordering::Relaxed);
            let discover = state
                .handles
                .discover_override
                .lock()
                .unwrap_or(state.options.discover);
            match discover {
                DiscoverBehavior::Modern => axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "result": {
                        "resultType": "complete",
                        "supportedVersions": ["2026-07-28"],
                        "capabilities": {"tools": {}},
                        "ttlMs": 0,
                        "cacheScope": "private",
                        "_meta": {
                            "io.modelcontextprotocol/serverInfo": {"name": "fake", "version": "0.0.0"}
                        },
                    },
                }))
                .into_response(),
                // A legacy server: the SEP-2575 probe reaches JSON-RPC dispatch
                // and gets method-not-found.
                DiscoverBehavior::MethodNotFound => {
                    axum::Json(err(-32601, "Method not found".to_string())).into_response()
                }
                DiscoverBehavior::NonJsonServerError => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "middleware exploded",
                )
                    .into_response(),
            }
        }
        Some("tools/list") => {
            if state.options.list_tools_hangs {
                std::future::pending::<()>().await;
            }
            axum::Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]},
            }))
            .into_response()
        }
        Some("tools/call") => {
            let n = state.handles.calls.fetch_add(1, Ordering::Relaxed);
            state.handles.call_bodies.lock().push(req.clone());
            state
                .handles
                .call_version_headers
                .lock()
                .push(header_values(
                    &headers,
                    axum::http::header::HeaderName::from_static("mcp-protocol-version"),
                ));
            let meta_version = meta_protocol_version(&req);
            // A legacy revision in the reserved `_meta` keys is -32022 on the C# and TypeScript
            // SDKs (this is the C# text), -32600 on Python; a reintroduced legacy-session stamp
            // must fail every legacy test here.
            if let Some(version) = meta_version
                && version < "2026-07-28"
            {
                return axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "error": {
                        "code": -32022,
                        "message": format!(
                            "Protocol version '{version}' requires the initialize handshake and cannot be selected through per-request metadata."
                        ),
                        "data": {"supported": ["2026-07-28"], "requested": version},
                    },
                }))
                .into_response();
            }
            let input_required = |result: serde_json::Value| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "result": result,
                })
            };
            match state.behavior {
                CallToolBehavior::ErrorThenOk { code } => {
                    if n == 0 {
                        axum::Json(err(code, "session expired".to_string())).into_response()
                    } else {
                        axum::Json(ok()).into_response()
                    }
                }
                CallToolBehavior::AlwaysError { code } => {
                    axum::Json(err(code, format!("attempt {}", n + 1))).into_response()
                }
                CallToolBehavior::HangThenOk { hang_ms } => {
                    if n == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                    }
                    axum::Json(ok()).into_response()
                }
                CallToolBehavior::ErrorThenHang { code, hang_ms } => {
                    if n == 0 {
                        axum::Json(err(code, "session expired".to_string())).into_response()
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                        axum::Json(ok()).into_response()
                    }
                }
                CallToolBehavior::StructuredOk => axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "result": {
                        "content": [{"type": "text", "text": FOLDERS_SUMMARY}],
                        "structuredContent": folders_payload(),
                    },
                }))
                .into_response(),
                CallToolBehavior::FormElicitThenOk => {
                    if n == 0 {
                        axum::Json(input_required(serde_json::json!({
                            "resultType": "input_required",
                            "inputRequests": {
                                "user_email": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "form",
                                        "message": "Please provide your email",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": {
                                                "email": {"type": "string", "format": "email"}
                                            },
                                            "required": ["email"]
                                        }
                                    }
                                }
                            },
                            "requestState": "sealed-round-1",
                        })))
                        .into_response()
                    } else {
                        axum::Json(ok()).into_response()
                    }
                }
                CallToolBehavior::FormElicitThenErrorThenOk => match n {
                    0 => axum::Json(input_required(serde_json::json!({
                        "resultType": "input_required",
                        "inputRequests": {
                            "user_email": {
                                "method": "elicitation/create",
                                "params": {
                                    "mode": "form",
                                    "message": "Please provide your email",
                                    "requestedSchema": {
                                        "type": "object",
                                        "properties": {
                                            "email": {"type": "string", "format": "email"}
                                        },
                                        "required": ["email"]
                                    }
                                }
                            }
                        },
                        "requestState": "sealed-round-1",
                    })))
                    .into_response(),
                    1 => axum::Json(err(-32603, "session expired".to_string())).into_response(),
                    _ => axum::Json(ok()).into_response(),
                },
                CallToolBehavior::UrlElicitThenStateOnlyThenOk => match n {
                    0 => axum::Json(input_required(serde_json::json!({
                        "resultType": "input_required",
                        "inputRequests": {
                            "user_auth": {
                                "method": "elicitation/create",
                                "params": {
                                    "mode": "url",
                                    "message": "Connect your account",
                                    "url": "https://example.com/connect",
                                    "elicitationId": "e-42"
                                }
                            }
                        },
                        "requestState": "sealed-url-1",
                    })))
                    .into_response(),
                    1 => axum::Json(input_required(serde_json::json!({
                        "resultType": "input_required",
                        "requestState": "sealed-url-2",
                    })))
                    .into_response(),
                    _ => axum::Json(ok()).into_response(),
                },
                CallToolBehavior::RequiresPerRequestCapabilities { code } => {
                    let has_elicitation = req
                        .pointer(
                            "/params/_meta/io.modelcontextprotocol~1clientCapabilities/elicitation",
                        )
                        .is_some_and(serde_json::Value::is_object);
                    if meta_version == Some("2026-07-28") && has_elicitation {
                        axum::Json(ok()).into_response()
                    } else {
                        axum::Json(err(code, "per-request client context required".to_string()))
                            .into_response()
                    }
                }
            }
        }
        _ => axum::http::StatusCode::ACCEPTED.into_response(),
    }
}

async fn fake_handle_get() -> axum::response::Response {
    use axum::response::IntoResponse;
    let body =
        axum::body::Body::from_stream(futures::stream::pending::<Result<String, std::io::Error>>());
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn spawn_test_http_server(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/mcp")
}

async fn spawn_fake_mcp(behavior: CallToolBehavior) -> (String, FakeMcpHandles) {
    spawn_fake_mcp_with(behavior, FakeMcpOptions::default()).await
}

/// A SEP-2575 modern server: `server/discover` succeeds and `initialize` is
/// never expected on the wire.
async fn spawn_fake_mcp_modern(behavior: CallToolBehavior) -> (String, FakeMcpHandles) {
    spawn_fake_mcp_with(
        behavior,
        FakeMcpOptions {
            discover: DiscoverBehavior::Modern,
            ..Default::default()
        },
    )
    .await
}

async fn spawn_fake_mcp_with(
    behavior: CallToolBehavior,
    options: FakeMcpOptions,
) -> (String, FakeMcpHandles) {
    let handles = FakeMcpHandles {
        inits: Arc::new(AtomicUsize::new(0)),
        discovers: Arc::new(AtomicUsize::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
        init_version: Arc::new(parking_lot::Mutex::new(None)),
        init_user_agents: Arc::new(parking_lot::Mutex::new(Vec::new())),
        init_capabilities: Arc::new(parking_lot::Mutex::new(None)),
        call_bodies: Arc::new(parking_lot::Mutex::new(Vec::new())),
        call_version_headers: Arc::new(parking_lot::Mutex::new(Vec::new())),
        discover_override: Arc::new(parking_lot::Mutex::new(None)),
    };
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::get(fake_handle_get).post(fake_handle_post),
        )
        .with_state(FakeMcpState {
            behavior,
            options,
            handles: handles.clone(),
        });
    (spawn_test_http_server(app).await, handles)
}

/// A client whose startup budget exceeds the `server/discover` probe timeout,
/// so the handshake runs the probe phase before any legacy fallback.
fn fake_http_client_probing(url: &str, tool_timeout_sec: u64) -> Arc<McpClient> {
    fake_http_client_with_startup(
        url,
        McpClient::DISCOVER_PROBE_TIMEOUT_SECS + 5,
        tool_timeout_sec,
    )
}

fn fake_http_client_with_startup(
    url: &str,
    startup_timeout_sec: u64,
    tool_timeout_sec: u64,
) -> Arc<McpClient> {
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(startup_timeout_sec),
        tool_timeout_sec: Some(tool_timeout_sec),
        ..Default::default()
    };
    fake_http_client_with_overrides(url, overrides)
}

fn fake_http_client(url: &str, tool_timeout_sec: u64) -> Arc<McpClient> {
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(5),
        tool_timeout_sec: Some(tool_timeout_sec),
        ..Default::default()
    };
    fake_http_client_with_overrides(url, overrides)
}

fn fake_http_client_with_overrides(
    url: &str,
    overrides: McpClientTimeoutOverrides,
) -> Arc<McpClient> {
    Arc::new(McpClient::new_http(
        "fake".to_string(),
        HttpConfig {
            url: url.to_string(),
            headers: vec![],
            local_agent_endpoint: false,
        },
        Some(&overrides),
        None,
    ))
}

fn fake_echo_tool() -> McpErasedTool {
    McpErasedTool {
        tool: McpTool::new(
            "echo".to_string(),
            "echo desc".to_string(),
            "fake".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({"type": "object"}),
            None,
        ),
    }
}

fn event_types(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_transport_sends_default_user_agent_on_initialize() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::HangThenOk { hang_ms: 0 }).await;
    let client = fake_http_client(&url, 5);
    client.ensure_initialized().await.expect("handshake");
    assert_eq!(
        *handles.init_user_agents.lock(),
        vec![format!("grok-cli/{}", distill_version::VERSION)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_mcperror_recovers_then_retry_succeeds() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::ErrorThenOk { code: -32603 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let tmp = tempfile::tempdir().unwrap();
    let ew = distill_session_events::EventWriter::open(tmp.path());

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let out = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("recovered call should succeed");

    assert!(
        !out.is_error.unwrap_or(false),
        "retry should return a success result"
    );
    assert!(reconnect, "reconnect_attempted must be set");
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "one failed + one retried tools/call"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "initial handshake + one recovery re-init"
    );
    assert_eq!(
        handles.init_version.lock().as_deref(),
        Some("2025-11-25"),
        "the legacy fallback must offer the newest initialize-era protocolVersion"
    );

    let jsonl = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
    let events = event_types(&jsonl);
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("mcp_transport_error")),
        "expected mcp_transport_error in {jsonl}"
    );
    assert!(
        events.iter().any(|e| {
            e.get("type").and_then(|t| t.as_str()) == Some("mcp_transport_reconnect")
                && e.get("success") == Some(&serde_json::Value::Bool(true))
        }),
        "expected a successful mcp_transport_reconnect in {jsonl}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_retry_failure_surfaces_retry_error() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32603 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect_err("both attempts fail");

    let msg = err.to_string();
    assert!(msg.contains("attempt 2"), "want retry error, got: {msg}");
    assert!(
        !msg.contains("attempt 1"),
        "must not surface the original error: {msg}"
    );
    assert!(reconnect);
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "one failed + one retried tools/call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_invalid_params_not_recovered() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32602 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect_err("invalid params surfaced as-is");

    assert!(err.to_string().contains("attempt 1"), "got: {err}");
    assert!(!reconnect, "invalid-params must not trigger recovery");
    assert!(!is_timeout);
    assert_eq!(handles.calls.load(Ordering::Relaxed), 1, "no retry POST");
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "no recovery re-init"
    );
}

/// Regression test for the version-strict-server startup failure: the fake rejects `server/discover` with method-not-found AND rejects any
/// `initialize` naming 2026-07-28. Startup must probe discover once, then fall back to a legacy `initialize` that names 2025-11-25 — never 2026-07-28, which SEP-2575 removed from the handshake.
#[tokio::test(flavor = "multi_thread")]
async fn handshake_falls_back_to_initialize_era_version_on_legacy_server() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32603 }).await;
    let client = fake_http_client_probing(&url, 5);

    client.ensure_initialized().await.expect("handshake");

    assert_eq!(
        handles.discovers.load(Ordering::Relaxed),
        1,
        "startup must probe server/discover once"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "legacy classification must fall back to one initialize"
    );
    assert_eq!(
        handles.init_version.lock().as_deref(),
        Some("2025-11-25"),
        "the fallback initialize must name the newest initialize-era protocolVersion"
    );
}

/// The probe phase shrinks to the startup budget instead of being skipped: a short budget
/// still negotiates modern servers, it just gives `server/discover` less time.
#[test]
fn probe_timeout_tracks_short_startup_budgets() {
    let url = "http://127.0.0.1:1/mcp";
    assert_eq!(
        fake_http_client_with_startup(url, 5, 5).probe_timeout_secs(),
        5
    );
    assert_eq!(
        fake_http_client_with_startup(url, McpClient::DISCOVER_PROBE_TIMEOUT_SECS, 5)
            .probe_timeout_secs(),
        McpClient::DISCOVER_PROBE_TIMEOUT_SECS
    );
    assert_eq!(
        fake_http_client_with_startup(url, McpClient::DISCOVER_PROBE_TIMEOUT_SECS + 5, 5)
            .probe_timeout_secs(),
        McpClient::DISCOVER_PROBE_TIMEOUT_SECS
    );
}

/// Waiters parked on an in-flight handshake must budget for both phases: the
/// holder can legitimately take probe + startup, so a `startup_timeout_sec`-only
/// wait would time out concurrent first tool calls.
#[test]
fn handshake_budget_covers_the_probe_phase() {
    let url = "http://127.0.0.1:1/mcp";
    assert_eq!(
        fake_http_client_with_startup(url, 5, 5).handshake_budget_secs(),
        10
    );
    let long = McpClient::DISCOVER_PROBE_TIMEOUT_SECS + 5;
    assert_eq!(
        fake_http_client_with_startup(url, long, 5).handshake_budget_secs(),
        long + McpClient::DISCOVER_PROBE_TIMEOUT_SECS
    );
}

/// Wire-level counterpart: a 5s-startup client (the desktop bind path's size class) still
/// probes `server/discover`, so a modern server is negotiated without any `initialize`.
#[tokio::test(flavor = "multi_thread")]
async fn short_startup_budget_still_negotiates_modern_servers() {
    let (url, handles) = spawn_fake_mcp_modern(CallToolBehavior::HangThenOk { hang_ms: 0 }).await;
    let client = fake_http_client(&url, 5);

    client.ensure_initialized().await.expect("handshake");

    assert_eq!(handles.discovers.load(Ordering::Relaxed), 1);
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        0,
        "a short-budget client must still reach the modern session"
    );
}

/// A short-budget client against a legacy server probes once, then runs the legacy
/// `initialize` naming 2025-11-25.
#[tokio::test(flavor = "multi_thread")]
async fn short_startup_budget_probes_then_falls_back_to_initialize() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32603 }).await;
    let client = fake_http_client(&url, 5);

    client.ensure_initialized().await.expect("handshake");

    assert_eq!(handles.discovers.load(Ordering::Relaxed), 1);
    assert_eq!(handles.inits.load(Ordering::Relaxed), 1);
    assert_eq!(
        handles.init_version.lock().as_deref(),
        Some("2025-11-25"),
        "the fallback initialize must name the newest initialize-era protocolVersion"
    );
}

/// A legacy server whose middleware reacts to the probe with a malformed shape
/// (non-JSON 500) must still handshake: the probe fails on its own transport and
/// the legacy `initialize` runs on a fresh one.
#[tokio::test(flavor = "multi_thread")]
async fn junk_discover_response_still_handshakes_via_initialize() {
    let (url, handles) = spawn_fake_mcp_with(
        CallToolBehavior::AlwaysError { code: -32603 },
        FakeMcpOptions {
            discover: DiscoverBehavior::NonJsonServerError,
            ..Default::default()
        },
    )
    .await;
    let client = fake_http_client_probing(&url, 5);

    client.ensure_initialized().await.expect("handshake");

    assert_eq!(handles.discovers.load(Ordering::Relaxed), 1);
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "a malformed probe reaction must still reach the initialize fallback"
    );
}

/// When the fallback `initialize` itself fails, its error must surface directly
/// — with its auth classification intact — rather than being hidden behind a
/// generic probe-then-fallback wrapper.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_fallback_surfaces_the_initialize_error_for_classification() {
    let (url, handles) = spawn_fake_mcp_with(
        CallToolBehavior::AlwaysError { code: -32603 },
        FakeMcpOptions {
            init_unauthorized: true,
            ..Default::default()
        },
    )
    .await;
    let client = fake_http_client_probing(&url, 5);

    let err = client
        .ensure_initialized()
        .await
        .expect_err("initialize is rejected");

    assert_eq!(handles.discovers.load(Ordering::Relaxed), 1);
    assert_eq!(handles.inits.load(Ordering::Relaxed), 1);
    assert!(
        err.is_auth_rejection(),
        "the initialize rejection must keep its auth classification: {err}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("Unauthorized"),
        "the initialize error text must surface to the user: {msg}"
    );
}

/// A SEP-2575 modern server: `server/discover` negotiates 2026-07-28, no `initialize` is
/// sent, and every request carries the client context in per-request `_meta`.
#[tokio::test(flavor = "multi_thread")]
async fn handshake_negotiates_modern_discover_without_initialize() {
    let (url, handles) = spawn_fake_mcp_modern(CallToolBehavior::HangThenOk { hang_ms: 0 }).await;
    let client = fake_http_client_probing(&url, 5);
    client.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));

    client.ensure_initialized().await.expect("handshake");

    assert_eq!(
        handles.discovers.load(Ordering::Relaxed),
        1,
        "startup must negotiate via server/discover"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        0,
        "a modern server must never receive initialize"
    );

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    let out = tool
        .try_call_tool(
            &client,
            &serde_json::json!({}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("tools/call succeeds over the modern session");
    assert!(!out.is_error.unwrap_or(false));
    let bodies = handles.call_bodies.lock().clone();
    assert!(!bodies.is_empty(), "tools/call must reach the server");
    assert_eq!(
        bodies
            .first()
            .and_then(|b| b.get("params"))
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
            .and_then(|v| v.as_str()),
        Some("2026-07-28"),
        "modern-era requests must carry the negotiated protocolVersion in _meta: {:?}",
        bodies.first()
    );
    assert!(
        bodies
            .first()
            .and_then(|body| body.pointer(
                "/params/_meta/io.modelcontextprotocol~1clientCapabilities/elicitation/form"
            ))
            .is_some_and(serde_json::Value::is_object),
        "modern-era requests carry the elicitation capability rmcp stamps: {:?}",
        bodies.first()
    );
    assert_eq!(
        handles.call_version_headers.lock().first(),
        Some(&vec!["2026-07-28".to_owned()]),
        "the header must name the negotiated modern version"
    );
}

/// `io.modelcontextprotocol/*` keys in a request body's `params._meta`, sorted.
fn reserved_meta_keys(body: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = body
        .pointer("/params/_meta")
        .and_then(serde_json::Value::as_object)
        .map(|meta| {
            meta.keys()
                .filter(|key| key.starts_with("io.modelcontextprotocol/"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    keys.sort();
    keys
}

fn meta_protocol_version(body: &serde_json::Value) -> Option<&str> {
    body.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(serde_json::Value::as_str)
}

/// Legacy `tools/call` must never carry `io.modelcontextprotocol/*` keys, bridge or not: the
/// fake rejects them the way the C# SDK does, so this also pins the wire shape on success.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_tools_call_never_carries_reserved_meta_keys() {
    for bridged in [true, false] {
        let (url, handles) = spawn_fake_mcp(CallToolBehavior::HangThenOk { hang_ms: 0 }).await;
        let client = fake_http_client(&url, 5);
        if bridged {
            client.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));
        }

        let tool = fake_echo_tool();
        let ew = distill_session_events::EventWriter::noop();
        let mut reconnect = false;
        let mut is_timeout = false;
        tool.try_call_tool(
            &client,
            &serde_json::json!({}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("tools/call succeeds over the legacy session");

        assert_eq!(handles.init_version.lock().as_deref(), Some("2025-11-25"));
        assert!(!reconnect, "bridged={bridged}: no recovery on a clean call");
        let bodies = handles.call_bodies.lock().clone();
        assert_eq!(
            bodies.iter().map(reserved_meta_keys).collect::<Vec<_>>(),
            vec![Vec::<String>::new()],
            "bridged={bridged}: legacy session must not stamp client context: {bodies:?}"
        );
        assert_eq!(
            handles.call_version_headers.lock().clone(),
            vec![vec!["2025-11-25".to_owned()]]
        );
    }
}

/// A 2026-07-28-era stateless server (C# SDK default) whose `server/discover` was
/// unreachable for the first handshake: the legacy session's `tools/call` is rejected
/// (-32021 missing capability, or -32022 unsupported protocol version), recovery re-probes,
/// lands on the modern session, and the retry carries the rmcp-stamped envelope the server
/// requires.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_session_rejected_by_modern_server_upgrades_via_recovery() {
    for code in [-32021, -32022] {
        let (url, handles) =
            spawn_fake_mcp(CallToolBehavior::RequiresPerRequestCapabilities { code }).await;
        let client = fake_http_client(&url, 5);
        client.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));
        client.ensure_initialized().await.expect("legacy handshake");
        assert_eq!(handles.discovers.load(Ordering::Relaxed), 1);
        assert_eq!(handles.init_version.lock().as_deref(), Some("2025-11-25"));
        *handles.discover_override.lock() = Some(DiscoverBehavior::Modern);

        let tool = fake_echo_tool();
        let ew = distill_session_events::EventWriter::noop();
        let mut reconnect = false;
        let mut is_timeout = false;
        let out = tool
            .try_call_tool(
                &client,
                &serde_json::json!({}),
                &mut reconnect,
                &mut is_timeout,
                &ew,
                &tracing::Span::none(),
            )
            .await
            .unwrap_or_else(|e| panic!("code {code}: tools/call succeeds after upgrading: {e}"));

        assert!(!out.is_error.unwrap_or(false));
        assert!(
            reconnect,
            "code {code}: the rejection must trigger recovery"
        );
        assert_eq!(handles.discovers.load(Ordering::Relaxed), 2);
        assert_eq!(
            handles.inits.load(Ordering::Relaxed),
            1,
            "code {code}: the recovery handshake must not fall back to initialize again"
        );
        let bodies = handles.call_bodies.lock().clone();
        assert_eq!(
            bodies.iter().map(meta_protocol_version).collect::<Vec<_>>(),
            vec![None, Some("2026-07-28")],
            "code {code}: one bare legacy call, then one upgraded call: {bodies:?}"
        );
        assert!(
            bodies
                .first()
                .is_some_and(|b| reserved_meta_keys(b).is_empty()),
            "code {code}: the legacy call stays bare: {bodies:?}"
        );
        assert!(
            bodies
                .get(1)
                .and_then(|b| b.pointer(
                    "/params/_meta/io.modelcontextprotocol~1clientCapabilities/elicitation"
                ))
                .is_some_and(serde_json::Value::is_object),
            "code {code}: the upgraded call carries the rmcp-stamped elicitation capability: {bodies:?}"
        );
        assert_eq!(
            handles.call_version_headers.lock().clone(),
            vec![vec!["2025-11-25".to_owned()], vec!["2026-07-28".to_owned()]]
        );
    }
}

/// The same server reached with `server/discover` available from the start never sees a
/// legacy request at all — the common path once the probe runs under every budget.
#[tokio::test(flavor = "multi_thread")]
async fn modern_server_requiring_per_request_capabilities_works_first_try() {
    let (url, handles) =
        spawn_fake_mcp_modern(CallToolBehavior::RequiresPerRequestCapabilities { code: -32021 })
            .await;
    let client = fake_http_client(&url, 5);
    client.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    tool.try_call_tool(
        &client,
        &serde_json::json!({}),
        &mut reconnect,
        &mut is_timeout,
        &ew,
        &tracing::Span::none(),
    )
    .await
    .expect("tools/call succeeds over the modern session");

    assert!(!reconnect);
    assert_eq!(handles.inits.load(Ordering::Relaxed), 0);
    assert_eq!(handles.calls.load(Ordering::Relaxed), 1);
}

/// Recovery that lands on a modern session carries 2026-07-28 in both `_meta` and the
/// `MCP-Protocol-Version` header, while the legacy call before it carried neither.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_onto_modern_session_adopts_modern_client_context() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::ErrorThenOk { code: -32603 }).await;
    let client = fake_http_client_probing(&url, 5);
    client.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));
    client.ensure_initialized().await.expect("legacy handshake");
    assert_eq!(handles.init_version.lock().as_deref(), Some("2025-11-25"));
    *handles.discover_override.lock() = Some(DiscoverBehavior::Modern);

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    tool.try_call_tool(
        &client,
        &serde_json::json!({}),
        &mut reconnect,
        &mut is_timeout,
        &ew,
        &tracing::Span::none(),
    )
    .await
    .expect("tools/call succeeds after recovery");

    assert!(reconnect, "the first call's error must trigger recovery");
    assert_eq!(handles.inits.load(Ordering::Relaxed), 1);
    let bodies = handles.call_bodies.lock().clone();
    let reserved: Vec<Vec<String>> = bodies.iter().map(reserved_meta_keys).collect();
    assert_eq!(
        reserved,
        vec![
            vec![],
            vec![
                "io.modelcontextprotocol/clientCapabilities".to_owned(),
                "io.modelcontextprotocol/clientInfo".to_owned(),
                "io.modelcontextprotocol/protocolVersion".to_owned(),
            ],
        ],
        "the legacy call stays bare; the recovered retry carries the modern envelope: {bodies:?}"
    );
    assert_eq!(
        bodies.iter().map(meta_protocol_version).collect::<Vec<_>>(),
        vec![None, Some("2026-07-28")],
        "the recovered retry must carry the modern session's version: {bodies:?}"
    );
    assert_eq!(
        handles.call_version_headers.lock().clone(),
        vec![vec!["2025-11-25".to_owned()], vec!["2026-07-28".to_owned()]]
    );
}

/// A connect-phase probe failure means the server was never reached; a legacy
/// retry against the same endpoint would only double the time-to-error, so the
/// probe surfaces the failure instead of returning a legacy verdict.
#[tokio::test(flavor = "multi_thread")]
async fn probe_connect_failure_surfaces_instead_of_legacy_fallback() {
    // A port that just stopped listening: connection refused at connect phase.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    drop(listener);

    let client = fake_http_client_probing(&url, 5);
    let config = HttpConfig {
        url: url.clone(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let http_client = McpClient::build_http_client(
        &config,
        "fake",
        crate::mcp_http_client::WarnBudget::default(),
    )
    .expect("client builds");
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        StreamableHttpClientTransportConfig::with_uri(url.as_str()),
    );

    let err = match client.probe_modern(transport).await {
        Err(err) => err,
        Ok(ProbeVerdict::Modern(_)) => panic!("no server is listening"),
        Ok(ProbeVerdict::Legacy { probe_error }) => {
            panic!("connect failures must not become a legacy verdict: {probe_error}")
        }
    };
    assert!(
        err.is_connect_failure(),
        "the surfaced error must classify as a connect failure: {err}"
    );
}

/// Deadline-driven callers derive per-server startup budgets from
/// [`McpClient::max_startup_within_deadline`]; the resulting worst-case
/// handshake must always fit the deadline.
#[test]
fn max_startup_within_deadline_fits_the_probe_phase() {
    let probe = McpClient::DISCOVER_PROBE_TIMEOUT_SECS;
    // Room for a full probe plus the legacy window: probe + startup == deadline.
    assert_eq!(McpClient::max_startup_within_deadline(30), 30 - probe);
    assert_eq!(McpClient::max_startup_within_deadline(2 * probe), probe);
    // Shorter deadlines split evenly: the probe timeout tracks the startup budget.
    assert_eq!(
        McpClient::max_startup_within_deadline(2 * probe - 1),
        probe - 1
    );
    assert_eq!(McpClient::max_startup_within_deadline(10), 5);
    assert_eq!(McpClient::max_startup_within_deadline(6), 3);
    let budget = |deadline: u64| {
        let startup = McpClient::max_startup_within_deadline(deadline);
        fake_http_client_with_startup("http://127.0.0.1:1/mcp", startup, 5).handshake_budget_secs()
    };
    for deadline in McpClient::MIN_HANDSHAKE_DEADLINE_SECS..=60 {
        assert!(
            budget(deadline) <= deadline,
            "deadline {deadline}s: worst-case handshake {}s exceeds it",
            budget(deadline)
        );
    }
    // Below the minimum, both one-second phases still need the minimum budget.
    for deadline in 0..McpClient::MIN_HANDSHAKE_DEADLINE_SECS {
        assert_eq!(budget(deadline), McpClient::MIN_HANDSHAKE_DEADLINE_SECS);
    }
}

/// End-to-end SEP-2322 form flow against the wire-level fake server:
/// `tools/call` → `input_required` (form elicitation + `requestState`) → HITL accept via the
#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_mrtr_form_elicitation_round_trip() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::FormElicitThenOk).await;
    let client = fake_http_client(&url, 5);
    let inbox = crate::elicitation::ElicitationInbox::new();
    client.set_elicitation_tx(Some(inbox.clone()));

    let answer = tokio::spawn(async move {
        let job = inbox
            .recv()
            .await
            .expect("elicitation job reaches the inbox");
        assert_eq!(job.server_name, "fake");
        assert_eq!(job.fields.message, "Please provide your email");
        assert!(
            matches!(
                job.fields.mode,
                distill_tools::mcp_elicitation::McpElicitModeFields::Form {
                    requested_schema: Some(_)
                }
            ),
            "form schema must survive the bridge"
        );
        let _ = job.response_tx.send(crate::elicitation::accept_result(Some(
            serde_json::json!({"email": "user@example.com"}),
        )));
    });

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    let out = tool
        .try_call_tool(
            &client,
            &serde_json::json!({"q": 1}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("tool call completes after the MRTR round");
    answer.await.unwrap();

    assert!(!out.is_error.unwrap_or(false));
    assert!(!reconnect, "MRTR rounds are not transport recovery");
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "initial call + one retry"
    );

    let bodies = handles.call_bodies.lock().clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        handles.init_version.lock().as_deref(),
        Some("2025-11-25"),
        "the legacy fallback must offer the newest initialize-era protocolVersion"
    );
    let init_caps = handles
        .init_capabilities
        .lock()
        .clone()
        .expect("initialize captured");
    assert!(
        init_caps
            .get("elicitation")
            .and_then(|e| e.get("form"))
            .is_some_and(|f| f.is_object())
            && init_caps
                .get("elicitation")
                .and_then(|e| e.get("url"))
                .is_some_and(|u| u.is_object()),
        "initialize must declare form+url elicitation: {init_caps}"
    );
    for body in &bodies {
        assert!(
            reserved_meta_keys(body).is_empty(),
            "legacy-session rounds carry no reserved _meta keys: {body}"
        );
    }
    assert!(
        bodies
            .first()
            .and_then(|b| b.get("params"))
            .and_then(|p| p.get("requestState"))
            .is_none(),
        "fresh call must not carry requestState"
    );
    let Some(retry) = bodies.get(1).and_then(|b| b.get("params")) else {
        panic!("expected retry params: {bodies:?}");
    };
    assert_eq!(
        retry.get("requestState").and_then(|v| v.as_str()),
        Some("sealed-round-1"),
        "requestState must be echoed verbatim"
    );
    assert_eq!(
        retry
            .get("inputResponses")
            .and_then(|r| r.get("user_email"))
            .and_then(|e| e.get("action"))
            .and_then(|a| a.as_str()),
        Some("accept")
    );
    assert_eq!(
        retry
            .get("inputResponses")
            .and_then(|r| r.get("user_email"))
            .and_then(|e| e.get("content"))
            .and_then(|c| c.get("email"))
            .and_then(|e| e.as_str()),
        Some("user@example.com")
    );
    assert_eq!(
        retry
            .get("arguments")
            .and_then(|a| a.get("q"))
            .and_then(|q| q.as_i64()),
        Some(1),
        "retry must re-send the original arguments"
    );
}

/// Transport recovery mid-MRTR: the retry after the elicitation round hits a recoverable
/// JSON-RPC error, recovery re-initializes the session, and the post-recovery retry (on the
/// fresh `RunningService`) still carries the `inputResponses` and echoed `requestState`.
#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_mrtr_round_survives_transport_recovery() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::FormElicitThenErrorThenOk).await;
    let client = fake_http_client(&url, 5);
    let inbox = crate::elicitation::ElicitationInbox::new();
    client.set_elicitation_tx(Some(inbox.clone()));

    let answer = tokio::spawn(async move {
        let job = inbox
            .recv()
            .await
            .expect("elicitation job reaches the inbox");
        let _ = job.response_tx.send(crate::elicitation::accept_result(Some(
            serde_json::json!({"email": "user@example.com"}),
        )));
    });

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    let out = tool
        .try_call_tool(
            &client,
            &serde_json::json!({}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("tool call completes after recovery");
    answer.await.unwrap();

    assert!(!out.is_error.unwrap_or(false));
    assert!(reconnect, "the failed retry must trigger recovery");
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        3,
        "initial call + failed retry + recovered retry"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "initial handshake + one recovery re-init"
    );
    let bodies = handles.call_bodies.lock().clone();
    let Some(recovered_retry) = bodies.get(2).and_then(|b| b.get("params")) else {
        panic!("expected recovered retry params: {bodies:?}");
    };
    assert_eq!(
        recovered_retry.get("requestState").and_then(|v| v.as_str()),
        Some("sealed-round-1"),
        "the recovered retry must still echo requestState"
    );
    assert_eq!(
        recovered_retry
            .get("inputResponses")
            .and_then(|r| r.get("user_email"))
            .and_then(|e| e.get("action"))
            .and_then(|a| a.as_str()),
        Some("accept")
    );
    assert!(
        bodies
            .get(2)
            .is_some_and(|b| reserved_meta_keys(b).is_empty()),
        "the retry on the re-initialized legacy session stays bare: {recovered_retry}"
    );
}

/// SEP-2322 URL mode: consent accept → retry (echoing state) → `requestState`-only
/// `input_required` while the out-of-band interaction is pending → retry → final result.
#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_mrtr_url_elicitation_with_state_only_round() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::UrlElicitThenStateOnlyThenOk).await;
    let client = fake_http_client(&url, 5);
    let inbox = crate::elicitation::ElicitationInbox::new();
    client.set_elicitation_tx(Some(inbox.clone()));

    let answer = tokio::spawn(async move {
        let job = inbox
            .recv()
            .await
            .expect("elicitation job reaches the inbox");
        let distill_tools::mcp_elicitation::McpElicitModeFields::Url {
            url,
            elicitation_id,
        } = &job.fields.mode
        else {
            panic!("expected url mode, got {:?}", job.fields.mode);
        };
        assert_eq!(url, "https://example.com/connect");
        assert_eq!(elicitation_id, "e-42");
        // URL-mode accept means "user consented to open the URL"; no content.
        let _ = job
            .response_tx
            .send(crate::elicitation::accept_result(None));
    });

    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();
    let mut reconnect = false;
    let mut is_timeout = false;
    let out = tool
        .try_call_tool(
            &client,
            &serde_json::json!({}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("tool call completes after the pending round resolves");
    answer.await.unwrap();

    assert!(!out.is_error.unwrap_or(false));
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        3,
        "initial call + consent retry + state-only retry"
    );

    let bodies = handles.call_bodies.lock().clone();
    let Some(consent_retry) = bodies.get(1).and_then(|b| b.get("params")) else {
        panic!("expected consent retry params: {bodies:?}");
    };
    assert_eq!(
        consent_retry.get("requestState").and_then(|v| v.as_str()),
        Some("sealed-url-1")
    );
    assert_eq!(
        consent_retry
            .get("inputResponses")
            .and_then(|r| r.get("user_auth"))
            .and_then(|e| e.get("action"))
            .and_then(|a| a.as_str()),
        Some("accept")
    );
    assert!(
        consent_retry
            .get("inputResponses")
            .and_then(|r| r.get("user_auth"))
            .and_then(|e| e.get("content"))
            .is_none_or(|c| c.is_null()),
        "url-mode accept carries no content"
    );
    let Some(state_only_retry) = bodies.get(2).and_then(|b| b.get("params")) else {
        panic!("expected state-only retry params: {bodies:?}");
    };
    assert_eq!(
        state_only_retry
            .get("requestState")
            .and_then(|v| v.as_str()),
        Some("sealed-url-2")
    );
    assert!(
        state_only_retry.get("inputResponses").is_none(),
        "state-only rounds retry without inputResponses"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn server_that_connects_then_never_lists_tools_times_out_within_its_budget() {
    let (url, _handles) = spawn_fake_mcp_with(
        CallToolBehavior::HangThenOk { hang_ms: 0 },
        FakeMcpOptions {
            list_tools_hangs: true,
            ..Default::default()
        },
    )
    .await;
    let client = fake_http_client_with_startup(&url, 2, 2);
    let state = Arc::new(Mutex::new(McpState::new(vec![])));

    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.get_tool_registrations(state),
    )
    .await
    .expect("the client bounds the list itself, well inside the outer guard");

    // The list budget is the handshake's worst case (2 startup + 2 probe) plus a list window of
    // at least the probe timeout (10); a handshake timeout would report 2.
    assert!(
        matches!(
            listed,
            Err(McpError::Timeout {
                timeout_secs: 14,
                ..
            })
        ),
        "a stalled tools/list must surface as the list budget's timeout, got {:?}",
        listed.as_ref().map(Vec::len)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_outer_timeout_resets_transport_no_retry() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::HangThenOk { hang_ms: 3000 }).await;
    let client = fake_http_client(&url, 1);
    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect_err("call must time out");

    assert!(err.to_string().contains("timed out"), "got: {err}");
    assert!(is_timeout, "is_timeout must be set");
    assert!(reconnect, "timeout arm flags the reconnect after resetting");
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        1,
        "timed-out call is NOT retried"
    );
    assert!(matches!(
        client.state_kind().await,
        ClientStateKind::Pending
    ));
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "no re-init during the timed-out dispatch"
    );

    let mut reconnect2 = false;
    let mut is_timeout2 = false;
    let out = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect2,
            &mut is_timeout2,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("second dispatch should re-init and succeed");
    assert!(!out.is_error.unwrap_or(false));
    assert!(!is_timeout2);
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "second dispatch re-initialized the session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_retry_timeout_surfaces_timeout() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::ErrorThenHang {
        code: -32603,
        hang_ms: 3000,
    })
    .await;
    let client = fake_http_client(&url, 1);
    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect_err("the retried call must time out");

    assert!(err.to_string().contains("timed out"), "got: {err}");
    assert!(is_timeout, "retry-timeout must set is_timeout");
    assert!(reconnect, "recovery was attempted");
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "the retry tools/call was attempted"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "recovery re-initialized before the retry"
    );
}

#[test]
fn test_new_stdio_has_no_http_config() {
    let client = McpClient::stub("stdio-srv");
    assert!(client.http_config.is_none());
}

#[tokio::test]
async fn test_reset_transport_succeeds_for_http_client() {
    let config = HttpConfig {
        url: "http://127.0.0.1:9/api/mcp".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let client = McpClient::new_http("example-mcp".to_string(), config, None, None);
    assert!(client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_fails_for_stub() {
    let client = McpClient::stub("stdio-srv");
    assert!(!client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_is_idempotent() {
    let config = HttpConfig {
        url: "http://127.0.0.1:9/api/mcp".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let client = McpClient::new_http("example-mcp".to_string(), config, None, None);

    assert!(client.reset_transport().await);
    assert!(client.reset_transport().await);
    assert!(client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_makes_ensure_initialized_retry_handshake() {
    let config = HttpConfig {
        url: "http://127.0.0.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let client = McpClient::new_http("test".to_string(), config, None, None);

    let err1 = client.ensure_initialized().await.unwrap_err();
    assert!(
        matches!(
            err1,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "first init should fail: {err1}"
    );

    assert!(client.reset_transport().await);

    let err2 = client.ensure_initialized().await.unwrap_err();
    assert!(
        matches!(
            err2,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "second init after reset should also attempt handshake: {err2}"
    );
}

#[tokio::test]
async fn recover_errors_for_client_with_no_restorable_transport() {
    let err = Arc::new(McpClient::stub("stdio"))
        .recover()
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::ClientError(_)), "got {err}");
}

#[tokio::test]
async fn reset_transport_rebuilds_acp_client() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let client = McpClient::new_acp(
        "sdk-tools".to_string(),
        "srv_0".to_string(),
        Arc::new(NoopInvoker),
        None,
        None,
    );

    assert!(client.reset_transport().await);
    assert!(
        matches!(
            &*client.state.lock().await,
            ClientState::Pending(PendingTransport::Acp { .. })
        ),
        "reset_transport should restore the ACP transport to Pending"
    );
}

#[tokio::test]
async fn try_call_tool_reconnects_then_succeeds_after_retriable_transport_error() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct EchoSdkServer;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for EchoSdkServer {
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
            let method = message
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": message
                        .get("params")
                        .and_then(|p| p.get("protocolVersion"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "echo", "version": "0.0.0" },
                }),
                "tools/call" => serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": message
                            .get("params")
                            .and_then(|p| p.get("arguments"))
                            .and_then(|a| a.get("text"))
                            .and_then(|t| t.as_str())
                            .unwrap_or_default(),
                    }],
                    "isError": false,
                }),
                other => return Err(format!("unexpected method {other}")),
            };
            Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
    }

    async fn dead_service() -> McpService {
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut writer = server_write;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if msg.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                    let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": msg
                            .get("params")
                            .and_then(|p| p.get("protocolVersion"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "dead", "version": "0.0.0" },
                    }});
                    let mut encoded = serde_json::to_string(&resp).unwrap();
                    encoded.push('\n');
                    let _ = writer.write_all(encoded.as_bytes()).await;
                    let _ = writer.flush().await;
                    let _ = reader.read_line(&mut line).await;
                    return;
                }
            }
        });
        let handler = GrokClientHandler {
            info: McpClient::make_client_info("dead", /* advertise_elicitation */ true),
            server_name: "dead".to_string(),
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
            elicitation_tx: Arc::new(parking_lot::Mutex::new(None)),
        };
        let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
            client_read,
            client_write,
        );
        Arc::new(
            handler
                .serve(transport)
                .await
                .expect("dead-service handshake"),
        )
    }

    let client = Arc::new(McpClient::new_acp(
        "sdk".to_string(),
        "srv_0".to_string(),
        Arc::new(EchoSdkServer),
        None,
        None,
    ));
    let dead = dead_service().await;
    *client.state.lock().await = ClientState::Ready {
        service: dead,
        _connected: MCP_SERVERS_CONNECTED.enter(),
    };

    let erased = McpErasedTool {
        tool: McpTool::new(
            "echo".to_string(),
            "echo".to_string(),
            "sdk".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({}),
            None,
        ),
    };

    let raw = serde_json::json!({ "text": "after reconnect" });
    let mut reconnect_attempted = false;
    let mut is_timeout = false;
    let ew = distill_session_events::EventWriter::noop();
    let result = erased
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect_attempted,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("retry after reconnect should succeed");

    assert_eq!(
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .expect("text content")
            .text,
        "after reconnect"
    );
    assert!(
        reconnect_attempted,
        "retriable transport error must set reconnect_attempted"
    );
    assert!(
        !is_timeout,
        "successful retry must not be flagged as timeout"
    );
    assert!(matches!(
        &*client.state.lock().await,
        ClientState::Ready { .. }
    ));
    assert!(
        MCP_SERVERS_CONNECTED.get() >= 1,
        "a Ready client must hold a connected-gauge slot"
    );
}

async fn watched_live_client(name: &str) -> Arc<McpClient> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (client_read, server_write) = tokio::io::duplex(64 * 1024);
    let (server_read, client_write) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut writer = server_write;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if msg.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": msg
                        .get("params")
                        .and_then(|p| p.get("protocolVersion"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "live", "version": "0.0.0" },
                }});
                let mut encoded = serde_json::to_string(&resp).unwrap();
                encoded.push('\n');
                let _ = writer.write_all(encoded.as_bytes()).await;
                let _ = writer.flush().await;
            }
        }
    });
    let handler = GrokClientHandler {
        info: McpClient::make_client_info(name, /* advertise_elicitation */ true),
        server_name: name.to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(None)),
        elicitation_tx: Arc::new(parking_lot::Mutex::new(None)),
    };
    let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
        client_read,
        client_write,
    );
    let service: McpService = Arc::new(
        handler
            .serve(transport)
            .await
            .expect("live-service handshake"),
    );

    let client = Arc::new(McpClient::new_http(
        name.to_string(),
        HttpConfig {
            url: "http://127.0.0.1:0/".to_string(),
            headers: Vec::new(),
            local_agent_endpoint: false,
        },
        None,
        None,
    ));
    *client.state.lock().await = ClientState::Ready {
        service,
        _connected: MCP_SERVERS_CONNECTED.enter(),
    };
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    client.set_event_tx(Some(event_tx));
    assert!(
        client
            .arm_liveness_watcher(std::time::Duration::from_secs(3600))
            .await,
        "watcher must arm on a healthy Ready client"
    );
    client
}

async fn assert_watcher_releases(weak: std::sync::Weak<McpClient>, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while weak.upgrade().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} must cancel the watcher and release its client Arc"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn evicting_a_watched_client_releases_the_watcher_arc() {
    let client = watched_live_client("evict").await;
    let weak = Arc::downgrade(&client);
    let mut owned = crate::owned_clients::OwnedClients::new();
    owned.insert("evict".to_string(), client);
    owned.remove("evict");
    assert_watcher_releases(weak, "eviction").await;
}

#[tokio::test]
async fn dropping_the_owned_map_releases_watched_clients() {
    let client = watched_live_client("teardown").await;
    let weak = Arc::downgrade(&client);
    let mut owned = crate::owned_clients::OwnedClients::new();
    owned.insert("teardown".to_string(), client);
    drop(owned);
    assert_watcher_releases(weak, "map teardown").await;
}

#[test]
fn is_auth_rejection_message_matches_auth_signals() {
    assert!(is_auth_rejection_message(
        "MCP server 'grok_com_notion' handshake failed: Auth required, when send initialize request"
    ));
    assert!(is_auth_rejection_message("401 Unauthorized"));
    assert!(is_auth_rejection_message("unauthorized"));
    assert!(is_auth_rejection_message("Authentication required"));
    assert!(is_auth_rejection_message("authentication failed"));
    assert!(is_auth_rejection_message("status: 401"));
    assert!(is_auth_rejection_message("HTTP status 401"));
    assert!(is_auth_rejection_message("server returned status code 401"));
    assert!(is_auth_rejection_message("HTTP 401"));
    assert!(is_auth_rejection_message("error 401"));
    assert!(is_auth_rejection_message(
        "worker quit with fatal: Transport channel closed, when Auth(AuthorizationRequired)"
    ));
    let auth_req = McpError::AuthRequired {
        server: "clickhouse".into(),
    };
    assert!(auth_req.is_auth_rejection());
    assert_eq!(auth_req.server_name(), Some("clickhouse"));
}

#[test]
fn auth_required_records_as_auth_not_init_failed_and_maps_category() {
    let mut state = McpState::new(vec![]);
    state.record_init_failure("oauth-srv", true, None);
    assert!(state.auth_required.contains("oauth-srv"));
    assert!(!state.init_failed.contains_key("oauth-srv"));

    let err = McpError::AuthRequired {
        server: "oauth-srv".into(),
    };
    assert!(matches!(
        err.error_category(),
        distill_session_events::McpErrorCategory::AuthRequired
    ));
}

#[test]
fn is_auth_rejection_message_rejects_non_auth() {
    assert!(!is_auth_rejection_message("Transport closed"));
    assert!(!is_auth_rejection_message(
        "MCP server 'x' timed out after 30s"
    ));
    assert!(!is_auth_rejection_message(
        "Failed to spawn MCP server 'x': No such file or directory"
    ));
    assert!(!is_auth_rejection_message("403 Forbidden"));
    assert!(!is_auth_rejection_message("forbidden"));
    assert!(!is_auth_rejection_message("request took 401ms"));
    assert!(!is_auth_rejection_message("connect 10.0.4.01:443"));
    assert!(!is_auth_rejection_message("read 401 bytes"));
    assert!(!is_auth_rejection_message("http 4012"));
    assert!(!is_auth_rejection_message("error 4012"));
    assert!(!is_auth_rejection_message("status: 4012"));
    assert!(!is_auth_rejection_message("http 401ms"));
    assert!(!is_auth_rejection_message("error 401ms"));
    assert!(is_auth_rejection_message("http 401."));
    assert!(is_auth_rejection_message("error 401: token expired"));
}

#[test]
fn mcp_error_is_auth_rejection_delegates() {
    assert!(McpError::ClientError("Auth required".to_string()).is_auth_rejection());
    assert!(!McpError::ClientError("Transport closed".to_string()).is_auth_rejection());
    assert!(
        !McpError::Timeout {
            server: "x".to_string(),
            timeout_secs: 30,
        }
        .is_auth_rejection()
    );
    assert!(
        !McpError::SpawnFailed {
            server: "x".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "401 Unauthorized"),
        }
        .is_auth_rejection()
    );
    assert!(
        McpError::HandshakeFailed {
            server: "x".to_string(),
            source: Box::new(ClientInitializeError::ConnectionClosed(
                "Auth required, when send initialize request".to_string()
            )),
        }
        .is_auth_rejection()
    );
    assert!(
        !McpError::HandshakeFailed {
            server: "x".to_string(),
            source: Box::new(ClientInitializeError::ConnectionClosed(
                "transport closed".to_string()
            )),
        }
        .is_auth_rejection()
    );
}

fn folders_payload() -> serde_json::Value {
    serde_json::json!({
        "folders": [
            {"id": "p1", "name": "Alpha"},
            {"id": "p2", "name": "Beta"},
            {"id": "c1", "name": "Custom One"},
        ]
    })
}

const FOLDERS_SUMMARY: &str = "7 product folders, 2 custom folders";

/// The render from `structured_content` onward is pinned in `call_result_tests`.
#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_structured_content_reaches_the_result() {
    let (url, _handles) = spawn_fake_mcp(CallToolBehavior::StructuredOk).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let ew = distill_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let result = tool
        .try_call_tool(
            &client,
            &serde_json::json!({}),
            &mut reconnect,
            &mut is_timeout,
            &ew,
            &tracing::Span::none(),
        )
        .await
        .expect("structured call should succeed");

    assert_eq!(Some(folders_payload()), result.structured_content);
}

#[test]
fn only_transport_service_errors_become_network_errors() {
    for transport in [
        ServiceError::TransportClosed,
        ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
            "stdio",
            std::any::TypeId::of::<()>(),
            Box::new(std::io::Error::other("broken pipe")),
        )),
    ] {
        assert_eq!(
            tool_error_for_service_error(&transport).kind,
            distill_tool_runtime::ToolErrorKind::NetworkError,
            "{transport}"
        );
    }
    for other in [
        mcp_service_err(-32603),
        ServiceError::UnexpectedResponse,
        ServiceError::Cancelled { reason: None },
    ] {
        assert_eq!(
            tool_error_for_service_error(&other).kind,
            distill_tool_runtime::ToolErrorKind::Custom,
            "{other}"
        );
    }
}

#[test]
fn structured_label_takes_short_strings_only() {
    let at_cap = "x".repeat(STRUCTURED_LABEL_MAX_LEN);
    let structured = serde_json::json!({
        "outcome": "session_busy",
        "mode": "remote",
        "versioned": "v1.2-rc.3",
        "at_cap": at_cap,
        "status": {"nested": true},
        "count": 3,
        "empty": "",
        "long": "x".repeat(STRUCTURED_LABEL_MAX_LEN + 1),
    });
    assert_eq!(
        structured_label(Some(&structured), "outcome").as_deref(),
        Some("session_busy")
    );
    assert_eq!(
        structured_label(Some(&structured), "mode").as_deref(),
        Some("remote")
    );
    assert_eq!(
        structured_label(Some(&structured), "versioned").as_deref(),
        Some("v1.2-rc.3")
    );
    assert_eq!(
        structured_label(Some(&structured), "at_cap").as_deref(),
        Some(at_cap.as_str()),
        "a label exactly at the cap is kept"
    );
    assert_eq!(
        structured_label(Some(&structured), "status"),
        None,
        "objects are data, not labels"
    );
    assert_eq!(structured_label(Some(&structured), "count"), None);
    assert_eq!(structured_label(Some(&structured), "empty"), None);
    assert_eq!(structured_label(Some(&structured), "long"), None);
    assert_eq!(structured_label(Some(&structured), "missing"), None);
    assert_eq!(structured_label(None, "outcome"), None);
}

#[test]
fn structured_label_drops_free_text() {
    let structured = serde_json::json!({
        "email": "jane@example.com",
        "name": "Jane Doe",
        "path": "/Users/user/file",
        "control": "ok\n",
        "unicode": "ok\u{e9}",
        "token": "sk-abc123==",
    });
    for key in ["email", "name", "path", "control", "unicode", "token"] {
        assert_eq!(
            structured_label(Some(&structured), key),
            None,
            "{key} is not label-shaped"
        );
    }
}

#[test]
fn load_expose_image_base64_defaults_to_false() {
    assert!(!McpClient::load_expose_image_base64(None, None));
}

#[test]
fn load_expose_image_base64_uses_overrides_when_meta_unset() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    assert!(McpClient::load_expose_image_base64(Some(&overrides), None));
}

#[test]
fn load_expose_image_base64_meta_wins_over_overrides() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let meta = McpServerMetaConfig {
        expose_image_base64: Some(false),
        ..Default::default()
    };
    assert!(!McpClient::load_expose_image_base64(
        Some(&overrides),
        Some(&meta)
    ));
}

#[test]
fn load_expose_image_base64_meta_falls_through_when_none() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let meta = McpServerMetaConfig::default();
    assert!(McpClient::load_expose_image_base64(
        Some(&overrides),
        Some(&meta)
    ));
}

#[test]
fn new_http_propagates_expose_image_base64_override_to_getter() {
    let config = HttpConfig {
        url: "http://localhost/api/mcp".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "grafana".to_string(),
        config.clone(),
        Some(&overrides),
        None,
    );
    assert!(client.expose_image_base64());

    let client_default = McpClient::new_http("grafana".to_string(), config, None, None);
    assert!(!client_default.expose_image_base64());
}

#[tokio::test]
async fn ensure_initialized_on_empty_client_returns_no_transport_error() {
    let client = McpClient::stub("test-server");

    let err = client.ensure_initialized().await.unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("no transport configured"),
        "expected clear 'no transport configured' error, got: {msg}"
    );
    assert!(
        !msg.contains("already initializing"),
        "regression: legacy fast-fail sentinel surfaced: {msg}"
    );
}

#[tokio::test]
async fn ensure_initialized_concurrent_callers_never_see_legacy_fast_fail() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let mut handles = Vec::new();
    for _ in 0..5 {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move { c.ensure_initialized().await }));
    }

    for (idx, handle) in handles.into_iter().enumerate() {
        let result = handle.await.expect("task did not panic");
        let err = result.expect_err("unreachable host must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("MCP client already initializing"),
            "caller {idx}: legacy fast-fail sentinel surfaced: {msg}"
        );
        assert!(
            matches!(
                err,
                McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
            ),
            "caller {idx}: expected handshake failure, got: {err}"
        );
    }
}

#[tokio::test]
async fn ensure_initialized_parked_caller_retries_after_notify() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config.clone(),
        Some(&overrides),
        None,
    ));

    *client.state.lock().await = ClientState::Initializing;

    let parker_client = Arc::clone(&client);
    let parker = tokio::spawn(async move { parker_client.ensure_initialized().await });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    *client.state.lock().await = ClientState::Pending(PendingTransport::Http(config.clone()));
    client.init_done.notify_waiters();

    let err = parker
        .await
        .expect("parker did not panic")
        .expect_err("unreachable host must fail");
    let msg = err.to_string();
    assert!(
        !msg.contains("MCP client already initializing"),
        "regression: legacy fast-fail sentinel: {msg}"
    );
    assert!(
        !msg.contains("init still in progress"),
        "parker should not hit wait-timeout when notified: {msg}"
    );
    assert!(
        matches!(
            err,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "expected handshake failure, got: {err}"
    );
}

#[tokio::test]
async fn ensure_initialized_inflight_wait_times_out_when_holder_silent() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(0),
        ..Default::default()
    };
    let client = McpClient::new_http("test-server".to_string(), config, Some(&overrides), None);

    *client.state.lock().await = ClientState::Initializing;

    let err = client.ensure_initialized().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("init still in progress"),
        "expected wait-timeout error, got: {msg}"
    );
    assert!(
        !msg.contains("already initializing"),
        "regression: legacy fast-fail sentinel: {msg}"
    );
}

#[tokio::test]
async fn ensure_initialized_drop_guard_restores_state_after_holder_aborted() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let holder_client = Arc::clone(&client);
    let holder = tokio::spawn(async move { holder_client.ensure_initialized().await });

    let started = std::time::Instant::now();
    loop {
        if matches!(&*client.state.lock().await, ClientState::Initializing) {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "holder never reached Initializing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    holder.abort();
    let _ = holder.await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    match &*client.state.lock().await {
        ClientState::Pending(_) => {}
        other => panic!(
            "expected Pending after holder abort + drop guard, found {}",
            state_label(other)
        ),
    }
}

#[test]
fn test_mcp_state_is_initialized_requires_empty_initializing_servers() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));

    let _owner = state.try_start_init().expect("claims init");
    state.mark_servers_initializing(["a".to_string()]);
    assert!(!state.is_initialized());
    assert!(state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(
        state.init_progress(),
        InitProgress::Starting { .. }
    ));

    state.finish_init();
    assert!(
        !state.is_initialized(),
        "is_initialized() must wait for per-server handshakes"
    );
    assert!(
        state.is_initializing(),
        "is_initializing() must report in-flight per-server work"
    );
    assert!(state.has_finished_init());
    assert!(state.is_server_handshaking("a"));
    assert_eq!(state.handshaking_servers_count(), 1);

    state.mark_server_ready("a");
    assert!(!state.is_server_handshaking("a"));
    assert_eq!(state.handshaking_servers_count(), 0);
    assert!(
        !state.is_initialized() && state.is_initializing(),
        "the last handshake landing is not completion"
    );
    state.complete_init();
    assert!(state.is_initialized());
    assert!(!state.is_initializing());
    assert!(state.has_finished_init());
}

#[test]
fn test_init_progress_state_machine_invariants() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    let _owner = state.try_start_init().expect("claims init");
    assert!(
        state.try_start_init().is_none(),
        "double try_start_init is rejected"
    );

    state.mark_servers_initializing(["a".to_string(), "b".to_string()]);
    assert_eq!(state.handshaking_servers_count(), 2);
    state.finish_init();
    state.mark_server_ready("a");
    state.mark_server_ready("b");
    assert!(
        !state.is_initialized() && state.is_initializing(),
        "the last handshake landing is not completion; the pass completes after its final publication"
    );
    state.complete_init();
    assert!(state.is_initialized() && !state.is_initializing());

    state.cancel_any_init();
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
    let _owner = state
        .try_start_init()
        .expect("cancel_any_init re-enables init");
}

fn state_label(s: &ClientState) -> &'static str {
    match s {
        ClientState::Empty => "Empty",
        ClientState::Pending(_) => "Pending",
        ClientState::Initializing => "Initializing",
        ClientState::Ready { .. } => "Ready",
    }
}

#[tokio::test]
async fn is_healthy_empty_returns_false() {
    let client = McpClient::stub("empty");
    assert!(matches!(*client.state.lock().await, ClientState::Empty));
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Empty);
}

#[tokio::test]
async fn is_healthy_pending_returns_false() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let client = McpClient::new_http("pending".to_string(), config, None, None);
    assert!(matches!(
        *client.state.lock().await,
        ClientState::Pending(_)
    ));
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Pending);
}

#[tokio::test]
async fn is_healthy_initializing_returns_false() {
    let client = McpClient::stub("initializing");
    *client.state.lock().await = ClientState::Initializing;
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Initializing);
}

#[tokio::test]
async fn is_healthy_pending_does_not_block_on_handshake() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
        local_agent_endpoint: false,
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "pending-unreachable".to_string(),
        config,
        Some(&overrides),
        None,
    );
    let start = std::time::Instant::now();
    let healthy = client.is_healthy().await;
    let elapsed = start.elapsed();
    assert!(!healthy);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "is_healthy must be a cheap state inspection, took {elapsed:?}"
    );
}

#[test]
fn make_client_info_pins_protocol_version() {
    // The client info's version rides the legacy `initialize` handshake, so it must
    // stay on the newest initialize-era revision: 2026-07-28 removed the handshake
    // (SEP-2575), and version-strict servers reject an `initialize` naming it.
    assert_eq!(
        McpClient::make_client_info("test-srv", /* advertise_elicitation */ true).protocol_version,
        rmcp::model::ProtocolVersion::V_2025_11_25
    );
}

#[test]
fn make_client_info_advertises_form_and_url_elicitation() {
    let info = McpClient::make_client_info("test-srv", /* advertise_elicitation */ true);
    let elicitation = info
        .capabilities
        .elicitation
        .as_ref()
        .expect("elicitation capability advertised");
    assert!(
        elicitation.form.is_some(),
        "form elicitation must be advertised"
    );
    assert!(
        elicitation.url.is_some(),
        "url elicitation must be advertised"
    );
    assert_eq!(
        elicitation.form.as_ref().and_then(|f| f.schema_validation),
        Some(true),
        "client validates form content before Accept"
    );
}

#[test]
fn acp_zero_ipc_client_info_does_not_advertise_elicitation() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let acp = McpClient::new_acp(
        "sdk".to_string(),
        "srv_0".to_string(),
        Arc::new(NoopInvoker),
        None,
        None,
    );
    let acp_info = acp.make_client_handler().get_info();
    assert!(
        acp_info.capabilities.elicitation.is_none(),
        "ACP zero-IPC cannot deliver elicitation/create"
    );

    let no_bridge = McpClient::stub("stdio");
    assert!(
        no_bridge
            .make_client_handler()
            .get_info()
            .capabilities
            .elicitation
            .is_none(),
        "stdio without an elicitation inbox must not advertise"
    );

    let hitl = McpClient::stub("stdio");
    hitl.set_elicitation_tx(Some(crate::elicitation::ElicitationInbox::new()));
    assert!(
        hitl.make_client_handler()
            .get_info()
            .capabilities
            .elicitation
            .is_some(),
        "stdio/HITL path with an inbox must still advertise elicitation"
    );
}

#[tokio::test]
async fn client_handler_routes_tools_changed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
    let handler = GrokClientHandler {
        info: McpClient::make_client_info("test", /* advertise_elicitation */ true),
        server_name: "test".to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(Some(tx))),
        elicitation_tx: Arc::new(parking_lot::Mutex::new(None)),
    };
    handler.emit(McpClientEvent::ToolsChanged {
        server: handler.server_name.clone(),
    });
    let ev = rx.recv().await.expect("event arrived");
    match ev {
        McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
        other => panic!("expected ToolsChanged, got {other:?}"),
    }
}

#[tokio::test]
async fn client_handler_observes_post_handshake_set_event_tx() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
    let client = Arc::new(McpClient::stub("test"));

    let handler = client.make_client_handler();

    assert!(handler.notify_tx.lock().is_none());

    client.set_event_tx(Some(tx));

    handler.emit(McpClientEvent::ToolsChanged {
        server: "test".to_string(),
    });
    let ev = rx.recv().await.expect("event arrived");
    match ev {
        McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
        other => panic!("expected ToolsChanged, got {other:?}"),
    }
}

#[derive(Clone)]
struct FakeStreamableHttpHandles {
    post_headers: Arc<parking_lot::Mutex<Option<axum::http::HeaderMap>>>,
}

async fn spawn_fake_streamable_http(
    post_status: axum::http::StatusCode,
) -> (String, FakeStreamableHttpHandles) {
    use axum::response::IntoResponse;
    let handles = FakeStreamableHttpHandles {
        post_headers: Arc::new(parking_lot::Mutex::new(None)),
    };
    let post_handles = handles.clone();
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::get(fake_handle_get).post(
            move |headers: axum::http::HeaderMap| async move {
                *post_handles.post_headers.lock() = Some(headers);
                post_status.into_response()
            },
        ),
    );
    (spawn_test_http_server(app).await, handles)
}

fn probe_ctx<'a>(
    event_writer: &'a distill_session_events::EventWriter,
    mode: OauthInteractivity,
) -> McpSpawnCtx<'a> {
    crate::isolate_distill_home_for_tests();
    McpSpawnCtx {
        session_id: None,
        event_writer,
        mode,
        scope: None,
        discovery: McpOauthDiscovery::Network,
        send_grok_agent_id_header: false,
    }
}

fn session_test_ctx(event_writer: &distill_session_events::EventWriter) -> McpSpawnCtx<'_> {
    crate::isolate_distill_home_for_tests();
    McpSpawnCtx::for_session(
        "sess",
        event_writer,
        OauthInteractivity::NonInteractive,
        None,
    )
}

const TEST_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

async fn resolve_tokenless_with_headers(
    url: &str,
    headers: &[(String, String)],
    mode: OauthInteractivity,
) -> HttpAuthDecision {
    let event_writer = distill_session_events::EventWriter::noop();
    let ctx = probe_ctx(&event_writer, mode);
    decide_http_auth_over_network("fake", url, headers, &ctx, TEST_DISCOVERY_TIMEOUT).await
}

async fn resolve_tokenless(url: &str, mode: OauthInteractivity) -> HttpAuthDecision {
    resolve_tokenless_with_headers(url, &[], mode).await
}

#[tokio::test(flavor = "multi_thread")]
async fn inconclusive_oauth_probe_connects_tokenless_streamable_http_headless() {
    let (url, _handles) = spawn_fake_streamable_http(axum::http::StatusCode::OK).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::NonInteractive).await;
    assert!(
        matches!(decision, HttpAuthDecision::NoOauthSupport),
        "tokenless streamable-http server must connect plain in non-interactive mode"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_access_probe_sends_default_user_agent() {
    let (url, handles) = spawn_fake_streamable_http(axum::http::StatusCode::OK).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::NonInteractive).await;
    assert!(matches!(decision, HttpAuthDecision::NoOauthSupport));

    let captured = handles
        .post_headers
        .lock()
        .take()
        .expect("probe POST must reach the fake server");
    assert_eq!(
        header_values(&captured, axum::http::header::USER_AGENT),
        vec![format!("grok-cli/{}", distill_version::VERSION)]
    );
    assert_eq!(
        header_values(&captured, axum::http::header::CONTENT_TYPE),
        vec!["application/json".to_string()]
    );
    assert_eq!(
        header_values(&captured, axum::http::header::ACCEPT),
        vec!["application/json, text/event-stream".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_access_probe_preserves_configured_user_agent() {
    let (url, handles) = spawn_fake_streamable_http(axum::http::StatusCode::OK).await;
    let headers = [
        ("User-Agent".to_string(), "custom-ua".to_string()),
        ("Content-Type".to_string(), "text/plain".to_string()),
        ("Accept".to_string(), "text/html".to_string()),
    ];
    let decision =
        resolve_tokenless_with_headers(&url, &headers, OauthInteractivity::NonInteractive).await;
    assert!(matches!(decision, HttpAuthDecision::NoOauthSupport));

    let captured = handles
        .post_headers
        .lock()
        .take()
        .expect("probe POST must reach the fake server");
    assert_eq!(
        header_values(&captured, axum::http::header::USER_AGENT),
        vec!["custom-ua".to_string()]
    );
    assert_eq!(
        header_values(&captured, axum::http::header::CONTENT_TYPE),
        vec!["application/json".to_string()]
    );
    assert_eq!(
        header_values(&captured, axum::http::header::ACCEPT),
        vec!["application/json, text/event-stream".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_access_probe_accepts_bad_request_reply() {
    let (url, _handles) = spawn_fake_streamable_http(axum::http::StatusCode::BAD_REQUEST).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::NonInteractive).await;
    assert!(matches!(decision, HttpAuthDecision::NoOauthSupport));
}

#[tokio::test(flavor = "multi_thread")]
async fn inconclusive_oauth_probe_stays_fail_closed_on_auth_challenge() {
    let (url, _handles) = spawn_fake_streamable_http(axum::http::StatusCode::UNAUTHORIZED).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::NonInteractive).await;
    assert!(
        matches!(decision, HttpAuthDecision::NeedsInteractiveLogin),
        "auth-challenging server must keep failing closed in non-interactive mode"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn inconclusive_oauth_probe_unreachable_fails_closed() {
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::get(fake_handle_get).post(|| async {
            futures::future::pending::<()>().await;
            axum::http::StatusCode::OK.into_response()
        }),
    );
    let url = spawn_test_http_server(app).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::NonInteractive).await;
    // A server nobody can reach is a connectivity verdict, not an auth one: still fails this spawn closed, but as the retryable `Unreachable`
    assert!(matches!(decision, HttpAuthDecision::Unreachable));
}

#[tokio::test(flavor = "multi_thread")]
async fn inconclusive_oauth_probe_connects_plain_interactively() {
    let (url, _handles) = spawn_fake_streamable_http(axum::http::StatusCode::UNAUTHORIZED).await;
    let decision = resolve_tokenless(&url, OauthInteractivity::Interactive).await;
    assert!(matches!(decision, HttpAuthDecision::NoOauthSupport));
}

#[tokio::test(flavor = "multi_thread")]
async fn inconclusive_oauth_probe_emits_timeout_and_verdict_events() {
    let (url, _handles) = spawn_fake_streamable_http(axum::http::StatusCode::OK).await;
    let tmp = tempfile::tempdir().unwrap();
    let event_writer = distill_session_events::EventWriter::open(tmp.path());
    let ctx = probe_ctx(&event_writer, OauthInteractivity::NonInteractive);
    let decision =
        decide_http_auth_over_network("fake", &url, &[], &ctx, TEST_DISCOVERY_TIMEOUT).await;
    assert!(matches!(decision, HttpAuthDecision::NoOauthSupport));

    let jsonl = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
    let events = event_types(&jsonl);
    let types: Vec<String> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    assert!(
        types.iter().any(|t| t == "mcp_oauth_discovery_timeout"),
        "missing timeout event; got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "mcp_oauth_probe_resolved"),
        "missing verdict event; got {types:?}"
    );
}

async fn spawn_counting_http_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_counter = Arc::clone(&counter);
    let app = axum::Router::new().fallback(move || {
        let c = Arc::clone(&handler_counter);
        async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            axum::http::StatusCode::NOT_FOUND
        }
    });
    (spawn_test_http_server(app).await, counter)
}

#[tokio::test(flavor = "multi_thread")]
async fn session_spawn_sends_zero_network_requests() {
    let (url, counter) = spawn_counting_http_server().await;
    let event_writer = distill_session_events::EventWriter::noop();

    let session_ctx = session_test_ctx(&event_writer);
    let client = start_mcp_server(
        make_http_server("fake", &url),
        None,
        None,
        None,
        &session_ctx,
    )
    .await
    .expect("plain HTTP client");
    assert!(!client.has_auth(), "no stored creds → plain HTTP client");
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "session spawn must perform zero network requests"
    );

    let login_ctx =
        session_test_ctx(&event_writer).with_oauth_discovery(McpOauthDiscovery::Network);
    let _ = start_mcp_server(make_http_server("fake", &url), None, None, None, &login_ctx)
        .await
        .expect("discovery resolves no-oauth against the counting fake");
    assert!(
        counter.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "login path still runs network discovery (and proves the counter sees requests)"
    );
}

#[test]
fn apply_stdio_env_session_id_cannot_be_shadowed() {
    let mut cmd = Command::new("true");
    let env = vec![acp::EnvVariable::new("GROK_SESSION_ID", "spoofed")];
    apply_stdio_env(&mut cmd, &env, Some("sess-real"));

    let value = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == "GROK_SESSION_ID")
        .and_then(|(_, v)| v)
        .map(|v| v.to_string_lossy().into_owned());
    assert_eq!(value.as_deref(), Some("sess-real"));
}

#[tokio::test]
async fn grok_agent_id_header_rejects_invalid_session_id() {
    let writer = distill_session_events::EventWriter::noop();
    let ctx = McpSpawnCtx::for_session(
        "bad\nsession",
        &writer,
        OauthInteractivity::Interactive,
        None,
    )
    .with_grok_agent_id_header();
    let server = acp::McpServer::Http(
        acp::McpServerHttp::new("local", "http://127.0.0.1:9/mcp").headers(vec![]),
    );

    let error = match start_mcp_server(server, None, None, None, &ctx).await {
        Ok(_) => panic!("invalid header value must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid X-Grok-Agent-ID value"));
}

#[test]
fn mcp_icon_from_rmcp_drops_empty_and_disallowed_src() {
    assert!(McpIcon::from_rmcp(rmcp::model::Icon::new("   ")).is_none());
    assert!(
        McpIcon::from_rmcp(rmcp::model::Icon::new("http://insecure.example/icon.png")).is_none()
    );
    assert!(McpIcon::from_rmcp(rmcp::model::Icon::new("javascript:alert(1)")).is_none());

    let icon = rmcp::model::Icon::new("https://example.com/icon.png")
        .with_mime_type("image/png")
        .with_sizes(vec!["48x48".to_string()])
        .with_theme(rmcp::model::IconTheme::Dark);
    let converted = McpIcon::from_rmcp(icon).unwrap();
    assert_eq!(converted.src, "https://example.com/icon.png");
    assert_eq!(converted.mime_type.as_deref(), Some("image/png"));
    assert_eq!(converted.sizes.as_deref(), Some(&["48x48".to_string()][..]));
    assert_eq!(converted.theme, Some(McpIconTheme::Dark));

    let padded = rmcp::model::Icon::new("  https://example.com/padded.png  ");
    assert_eq!(
        McpIcon::from_rmcp(padded).unwrap().src,
        "https://example.com/padded.png"
    );

    let data = rmcp::model::Icon::new("data:image/png;base64,aaa");
    assert!(McpIcon::from_rmcp(data).is_some());
}

#[test]
fn mcp_icon_from_rmcp_list_caps_count_and_src_bytes() {
    let many: Vec<_> = (0..20)
        .map(|i| rmcp::model::Icon::new(format!("https://example.com/{i}.png")))
        .collect();
    assert_eq!(
        McpIcon::from_rmcp_list(Some(many)).len(),
        MAX_MCP_ICONS_PER_ENTITY
    );

    let huge = format!("https://example.com/{}", "x".repeat(MAX_MCP_ICON_SRC_BYTES));
    assert!(McpIcon::from_rmcp(rmcp::model::Icon::new(huge)).is_none());
}

#[test]
fn mcp_icon_from_rmcp_caps_mime_type_and_sizes() {
    let long_mime = "a".repeat(MAX_MCP_ICON_MIME_TYPE_BYTES + 1);
    let converted = McpIcon::from_rmcp(
        rmcp::model::Icon::new("https://example.com/icon.png").with_mime_type(long_mime),
    )
    .unwrap();
    assert_eq!(converted.mime_type, None);

    let many_sizes: Vec<_> = (0..20).map(|i| format!("{i}x{i}")).collect();
    let converted = McpIcon::from_rmcp(
        rmcp::model::Icon::new("https://example.com/icon.png").with_sizes(many_sizes),
    )
    .unwrap();
    assert_eq!(
        converted.sizes.as_ref().map(|s| s.len()),
        Some(MAX_MCP_ICON_SIZES)
    );

    let long_token = "x".repeat(MAX_MCP_ICON_SIZE_TOKEN_BYTES + 1);
    let converted = McpIcon::from_rmcp(
        rmcp::model::Icon::new("https://example.com/icon.png")
            .with_sizes(vec![long_token, "48x48".to_string()]),
    )
    .unwrap();
    assert_eq!(converted.sizes.as_deref(), Some(&["48x48".to_string()][..]));
}

#[test]
fn record_tool_icons_insert_empty_removes() {
    let mut state = McpState::new(vec![]);
    let name = "server__tool".to_string();
    let icons = vec![McpIcon {
        src: "https://example.com/a.png".to_string(),
        mime_type: None,
        sizes: None,
        theme: None,
    }];
    state.record_tool_icons(name.clone(), icons);
    assert_eq!(state.mcp_tool_icons.get(&name).map(|v| v.len()), Some(1));
    state.record_tool_icons(name.clone(), Vec::new());
    assert!(!state.mcp_tool_icons.contains_key(&name));
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_spawn_guard_kills_grandchildren() {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 600 & echo $!; wait"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    distill_tools::util::detach_command(&mut cmd);
    #[allow(clippy::disallowed_methods)]
    let mut child = cmd.spawn().expect("spawn wrapper");
    let mut group = ProcessGroup::new().expect("group");
    group.attach(&child).expect("attach");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .await
        .expect("read grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("parse grandchild pid");
    let guard = SpawnGuard::new(child, Some(Arc::new(group)));
    assert!(
        unix_process_exists(grandchild),
        "grandchild must be alive before the guard drops"
    );

    drop(guard);

    let deadline = Instant::now() + Duration::from_secs(5);
    while unix_process_exists(grandchild) {
        assert!(
            Instant::now() < deadline,
            "grandchild {grandchild} survived the guard drop"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_into_a_closed_scope_fails_fast() {
    let scope = ProcessScope::new();
    scope.kill_all();

    let mut cmd = Command::new("sleep");
    cmd.arg("600").kill_on_drop(true);
    distill_tools::util::detach_command(&mut cmd);
    let result = SafeTokioChildProcess::spawn(
        cmd,
        Some(&scope),
        "closed-scope".to_string(),
        distill_session_events::EventWriter::noop(),
    )
    .await;

    assert!(
        result.is_err(),
        "spawning into a closed scope must fail fast, not start a doomed server"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Unreachable-spawn retry (McpError::Unreachable + McpState cooldown gating)
// ═══════════════════════════════════════════════════════════════════════════

/// `Unreachable` is a connectivity verdict: never auth, always retryable, still attributable to its server for failure recording.
#[test]
fn unreachable_error_is_retryable_not_auth() {
    let err = McpError::Unreachable {
        server: "srv".to_string(),
    };
    assert!(!err.is_auth_rejection());
    assert!(err.is_unreachable());
    assert_eq!(err.server_name(), Some("srv"));
    // AuthRequired stays the only pre-spawn auth gate.
    assert!(
        McpError::AuthRequired {
            server: "srv".to_string()
        }
        .is_auth_rejection()
    );
}

/// Candidates are returned only when the cooldown expired AND the server is still configured.
/// A taken candidate is in-flight (holding an attempt token) and is not handed to another trigger until its attempt settles.
#[test]
fn unreachable_retry_candidates_respect_cooldown_and_config() {
    let mut state = McpState::new(vec![
        make_stdio_server("due", "/bin/due"),
        make_stdio_server("cooling", "/bin/cooling"),
    ]);
    let now = std::time::Instant::now();
    state.record_unreachable_failure_at("due", "conn refused".to_string(), now);
    state.record_unreachable_failure_at(
        "cooling",
        "conn refused".to_string(),
        now + McpState::UNREACHABLE_RETRY_COOLDOWN,
    );
    // A server no longer in the config list is never a candidate.
    state.record_unreachable_failure_at("unconfigured", "conn refused".to_string(), now);

    let taken = state.take_unreachable_retry_candidates();
    assert_eq!(taken.len(), 1);
    let Some((name, token)) = taken.first().cloned() else {
        panic!("expected a retry candidate: {taken:?}");
    };
    assert_eq!(name, "due");
    // Both failures land in init_failed (Unavailable in status snapshots)
    assert!(state.init_failed.contains_key("due"));
    assert!(state.init_failed.contains_key("cooling"));

    // In-flight: no other trigger can take the same server, even though the attempt may outlive any fixed cooldown
    assert!(state.take_unreachable_retry_candidates().is_empty());
    // Recording from the init path never demotes an in-flight attempt.
    state.record_unreachable_failure_at("due", "racing".to_string(), now);
    assert!(state.take_unreachable_retry_candidates().is_empty());

    // A failed settle restarts the cooldown from settle time (still cooling now), refreshing the detail
    state.settle_unreachable_attempt_failed("due", token, "still down".to_string());
    assert_eq!(
        state.init_failed.get("due").map(String::as_str),
        Some("still down")
    );
    assert!(state.take_unreachable_retry_candidates().is_empty());

    // Success path: finish requires the token and clears the failure record.
    state.record_unreachable_failure_at("due", "again".to_string(), now);
    let taken = state.take_unreachable_retry_candidates();
    let Some((name, token)) = taken.first().cloned() else {
        panic!("expected a retry candidate: {taken:?}");
    };
    assert!(state.finish_unreachable_attempt(&name, token));
    assert!(!state.init_failed.contains_key("due"));
    // Settling twice (or with a stale token) is a no-op.
    assert!(!state.finish_unreachable_attempt(&name, token));
}

/// Config teardown mid-attempt invalidates the attempt token.
/// A stale attempt can neither install (finish returns false) nor re-pollute the records its teardown cleaned (failed/unretryable settles no-op).
#[test]
fn unreachable_attempt_token_invalidated_by_config_teardown() {
    let mut state = McpState::new(vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("gone", "/bin/gone"),
    ]);
    let now = std::time::Instant::now();
    state.record_unreachable_failure_at("gone", "down".to_string(), now);
    let taken = state.take_unreachable_retry_candidates();
    let Some((name, token)) = taken.first().cloned() else {
        panic!("expected a retry candidate: {taken:?}");
    };
    assert_eq!(name, "gone");

    // The server is removed from config while the attempt is in flight.
    state
        .update_configs_diff(vec![make_stdio_server("keep", "/bin/keep")])
        .expect("should detect change");
    assert!(!state.init_failed.contains_key("gone"));

    assert!(!state.finish_unreachable_attempt("gone", token));
    assert!(!state.settle_unreachable_attempt_unretryable("gone", token));
    state.settle_unreachable_attempt_failed("gone", token, "stale".to_string());
    assert!(!state.init_failed.contains_key("gone"));
    assert!(state.take_unreachable_retry_candidates().is_empty());
}

/// Config updates must not leave a stale unreachable schedule behind: the diff path forgets removed servers, the full-replace path clears everything.
#[test]
fn unreachable_schedule_cleared_on_config_updates() {
    let mut state = McpState::new(vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("remove", "/bin/remove"),
    ]);
    let now = std::time::Instant::now();
    state.record_unreachable_failure_at("keep", "down".to_string(), now);
    state.record_unreachable_failure_at("remove", "down".to_string(), now);

    let diff = state
        .update_configs_diff(vec![make_stdio_server("keep", "/bin/keep")])
        .expect("should detect change");
    assert_eq!(diff.removed, vec!["remove"]);
    assert!(!state.init_failed.contains_key("remove"));
    // The removed server is no longer scheduled; the kept server still is
    let taken: Vec<McpServerName> = state
        .take_unreachable_retry_candidates()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(taken, vec!["keep".to_string()]);

    // A full replace clears the whole schedule (including in-flight attempts)
    assert!(state.update_configs(vec![make_stdio_server("keep", "/bin/keep2")]));
    assert!(state.take_unreachable_retry_candidates().is_empty());
    assert!(state.init_failed.is_empty());
}

/// A fresh init attempt (`mark_servers_initializing`) supersedes any pending unreachable-respawn schedule.
/// The attempt itself re-records on failure, so a server recovered by a full re-init cannot be respawned over later.
#[test]
fn fresh_init_attempt_clears_unreachable_schedule() {
    let mut state = McpState::new(vec![make_stdio_server("srv", "/bin/srv")]);
    state.record_unreachable_failure_at("srv", "down".to_string(), std::time::Instant::now());

    state.mark_servers_initializing(vec!["srv".to_string()]);
    assert!(!state.init_failed.contains_key("srv"));
    assert!(state.take_unreachable_retry_candidates().is_empty());
}

/// An attempt whose future was cancelled never settles.
/// After the lease expires the server is takeable again with a fresh token and the zombie attempt's old token no-ops.
#[test]
fn unreachable_attempt_lease_reclaims_cancelled_attempts() {
    let mut state = McpState::new(vec![make_stdio_server("srv", "/bin/srv")]);
    let now = std::time::Instant::now();
    state.record_unreachable_failure_at("srv", "down".to_string(), now);
    let taken = state.take_unreachable_retry_candidates_at(now);
    let Some((_, old_token)) = taken.first().cloned() else {
        panic!("expected a retry candidate: {taken:?}");
    };

    // Within the lease the attempt is exclusive.
    assert!(
        state
            .take_unreachable_retry_candidates_at(now + McpState::UNREACHABLE_ATTEMPT_LEASE)
            .is_empty()
    );

    // Past the lease the server is handed out again with a new token…
    let later = now + McpState::UNREACHABLE_ATTEMPT_LEASE + std::time::Duration::from_secs(1);
    let taken = state.take_unreachable_retry_candidates_at(later);
    assert_eq!(taken.len(), 1);
    let Some((name, new_token)) = taken.first().cloned() else {
        panic!("expected a retry candidate: {taken:?}");
    };
    assert_eq!(name, "srv");
    assert_ne!(new_token, old_token);

    // …and the zombie attempt's token is dead: it cannot install or settle.
    assert!(!state.finish_unreachable_attempt("srv", old_token));
    state.settle_unreachable_attempt_failed("srv", old_token, "stale".to_string());
    // The server is still owned by the new attempt (settle with the new token works)
    assert!(state.finish_unreachable_attempt("srv", new_token));
}

/// Typed verdicts and transport-level handshake/client errors are retryable; protocol junk and auth are not.
#[test]
fn transient_connectivity_classification() {
    assert!(
        McpError::Unreachable {
            server: "srv".to_string()
        }
        .is_transient_connectivity()
    );
    assert!(
        McpError::Timeout {
            server: "srv".to_string(),
            timeout_secs: 65
        }
        .is_transient_connectivity()
    );
    assert!(
        McpError::ClientError("transport error: Connection reset by peer (os error 104)".into())
            .is_transient_connectivity()
    );
    assert!(
        McpError::ClientError("error sending request for url (http://x/mcp)".into())
            .is_transient_connectivity()
    );
    // Protocol-level and auth failures stay non-retryable.
    assert!(
        !McpError::ClientError("invalid params: missing field `tools`".into())
            .is_transient_connectivity()
    );
    assert!(
        !McpError::AuthRequired {
            server: "srv".to_string()
        }
        .is_transient_connectivity()
    );
    // The connect-failure subset excludes timeouts and mid-stream drops.
    assert!(
        McpError::Unreachable {
            server: "srv".to_string()
        }
        .is_connect_failure()
    );
    assert!(
        McpError::ClientError("tcp connect error: Connection refused (os error 61)".into())
            .is_connect_failure()
    );
    assert!(
        !McpError::Timeout {
            server: "srv".to_string(),
            timeout_secs: 65
        }
        .is_connect_failure()
    );
    assert!(
        !McpError::ClientError("transport error: Connection reset by peer (os error 104)".into())
            .is_connect_failure()
    );
    assert!(
        !McpError::ClientError("request timed out awaiting the server response".into())
            .is_connect_failure(),
        "post-connect timeouts stay out of the connect-phase class"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_connect_handshake_classifies_connect_phase() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    drop(listener);

    let event_writer = distill_session_events::EventWriter::noop();
    let ctx = session_test_ctx(&event_writer);
    let client = start_mcp_server(make_http_server("refused", &url), None, None, None, &ctx)
        .await
        .expect("a plain HTTP client builds without network");
    let err = client
        .ensure_initialized()
        .await
        .expect_err("a refused port must fail the handshake");
    assert!(
        err.is_connect_failure(),
        "a real refused-connect handshake must classify connect-phase: {err}"
    );
}

#[test]
fn has_client_is_identity_not_name() {
    let mut state = McpState::new(vec![]);
    let first = Arc::new(McpClient::stub("srv"));
    let replacement = Arc::new(McpClient::stub("srv"));
    state
        .owned_clients
        .insert("srv".to_owned(), Arc::clone(&first));
    assert!(state.has_client("srv", &first));
    state
        .owned_clients
        .insert("srv".to_owned(), Arc::clone(&replacement));
    assert!(
        !state.has_client("srv", &first),
        "tools listed from a replaced client must not land under its successor's name"
    );
    assert!(state.has_client("srv", &replacement));
}

#[tokio::test]
async fn slot_gate_lands_only_on_the_slot_the_caller_saw() {
    use crate::shared_mcp_state::SharedMcpState;
    let config = make_stdio_server("srv", "/bin/srv");
    let state = Arc::new(Mutex::new(McpState::new(vec![config.clone()])));
    let seen = state.lock().await.current_generation();
    // Removed and re-added with identical bytes: the slot is empty and the config compares equal.
    assert!(state.lock().await.update_configs(vec![]));
    assert!(state.lock().await.update_configs(vec![config]));
    assert_eq!(
        state.write_if_slot_is("srv", None, &seen, |_| ()).await,
        Err(Superseded),
        "an install authorized before the change must not land after it"
    );
    let current = state.lock().await.current_generation();
    let claim = state.lock().await.try_start_init().expect("claims init");
    assert_eq!(
        state.write_if_slot_is("srv", None, &current, |_| ()).await,
        Err(Superseded),
        "an empty slot a live pass may still fill is not the caller's to take"
    );
    drop(claim);
    assert_eq!(
        state.write_if_slot_is("srv", None, &current, |_| ()).await,
        Ok(())
    );
    let first = Arc::new(McpClient::stub("srv"));
    state
        .lock()
        .await
        .owned_clients
        .insert("srv".to_owned(), Arc::clone(&first));
    assert_eq!(
        state.write_if_slot_is("srv", None, &current, |_| ()).await,
        Err(Superseded),
        "an install into a slot another owner filled is refused"
    );
    assert_eq!(
        state
            .write_if_slot_is("srv", Some(&first), &current, |_| ())
            .await,
        Ok(()),
        "a replacement lands while the slot still holds the client it saw"
    );
    state
        .lock()
        .await
        .owned_clients
        .insert("srv".to_owned(), Arc::new(McpClient::stub("srv")));
    assert_eq!(
        state
            .write_if_slot_is("srv", Some(&first), &current, |_| ())
            .await,
        Err(Superseded),
        "a replacement never lands over a newer owner's client"
    );
}

#[test]
fn init_is_reclaimable_when_the_pass_dies_after_finish_init() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    let claim = state.try_start_init().expect("claims init");
    state.mark_servers_initializing(["a".to_owned()]);
    state.finish_init();
    assert!(
        state.is_initializing() && state.try_start_init().is_none(),
        "a live pass keeps init through finish_init"
    );
    drop(claim);
    assert!(state.is_init_abandoned());
    assert!(
        state.try_start_init().is_some(),
        "a dead pass with handshakes still marked is re-owned, not waited on"
    );
}

#[test]
fn restart_init_hands_the_caller_the_claim() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    let before = state.current_generation();
    let claim = state.restart_init();
    assert!(
        before.is_cancelled() && state.is_initializing() && state.try_start_init().is_none(),
        "waiters park instead of starting a pass of their own"
    );
    drop(claim);
    assert!(state.try_start_init().is_some());
}

#[test]
fn replaced_generation_names_what_replaced_it() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    let current = state.current_generation();
    assert_eq!(current.replaced_by(), None);
    drop(state.restart_init());
    assert_eq!(current.replaced_by(), Some(Replacement::Rebuild));
    let current = state.current_generation();
    assert!(state.update_configs(vec![]));
    assert_eq!(current.replaced_by(), Some(Replacement::ServerSetChange));
}

#[test]
fn config_change_releases_every_init_claim() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    let claim = state.try_start_init().expect("claims init");
    assert!(state.update_configs(vec![make_stdio_server("b", "/bin/b")]));
    assert!(
        !state.owns_init(&claim),
        "a holder that awaited across the change must not act on its claim"
    );
}

#[test]
fn config_change_hands_its_successor_the_claim() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    let change = state
        .change_configs(vec![make_stdio_server("b", "/bin/b")])
        .expect("the set changed");
    assert!(
        state.owns_init(&change.claim) && state.try_start_init().is_none(),
        "no waiter can start a pass between the change and its own"
    );
    assert!(
        state.is_server_pending("b"),
        "until the successor seeds its set, every server may still be handed a client"
    );
    assert!(
        state
            .change_configs(vec![make_stdio_server("b", "/bin/b")])
            .is_none(),
        "an unchanged set hands out nothing"
    );
}

#[test]
fn only_the_pass_s_own_servers_stay_pending_once_seeded() {
    let mut state = McpState::new(vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ]);
    state.auth_required.insert("a".to_owned());
    let _claim = state.try_start_init().expect("claims init");
    state.mark_servers_initializing(["a".to_owned()]);
    assert!(
        state.is_server_pending("a") && !state.is_server_pending("b"),
        "a sign-in for a server the pass does not touch is not held up by it"
    );
    assert!(
        !state.auth_required.contains("a"),
        "a fresh attempt clears the prior sign-in verdict; a failure re-records it"
    );
    state.mark_server_ready("a");
    assert!(!state.is_server_pending("a"));
}

#[tokio::test]
async fn write_on_a_replaced_premise_is_refused() {
    use crate::shared_mcp_state::SharedMcpState;
    let state = Arc::new(Mutex::new(McpState::new(vec![])));
    let stale = state.lock().await.current_generation();
    assert!(
        state
            .lock()
            .await
            .update_configs(vec![make_stdio_server("c", "/bin/c")])
    );
    let claim = state.lock().await.try_start_init().expect("claims init");
    assert!(
        state
            .lock()
            .await
            .update_configs(vec![make_stdio_server("d", "/bin/d")])
    );

    let stale_write = state
        .write_if_current(&stale, |state| {
            state.mark_servers_initializing(["ghost".to_owned()])
        })
        .await;
    let released_claim = state
        .write_if_owner(claim, |state, _claim| state.finish_init())
        .await;
    let current = state.lock().await;
    assert_eq!(stale_write, Err(Superseded));
    assert_eq!(released_claim, Err(Superseded));
    assert!(
        !current.is_server_handshaking("ghost") && !current.has_finished_init(),
        "a stale generation and a released claim write nothing"
    );
}

/// A scripted MCP server over an in-memory duplex that answers `initialize`, records every
/// message it receives, and answers `tools/call` only when `reply` is set.
async fn recording_service(
    reply: bool,
) -> (McpService, Arc<parking_lot::Mutex<Vec<serde_json::Value>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let received: Arc<parking_lot::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let (client_read, server_write) = tokio::io::duplex(64 * 1024);
    let (server_read, client_write) = tokio::io::duplex(64 * 1024);
    let sink = received.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut writer = server_write;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            sink.lock().push(msg.clone());
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let response = match msg.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => {
                    Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": msg.pointer("/params/protocolVersion"),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "recording", "version": "0.0.0" },
                    }}))
                }
                Some("tools/call") if reply => {
                    Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "content": [{ "type": "text", "text": "done" }],
                        "isError": false,
                    }}))
                }
                _ => None,
            };
            if let Some(response) = response {
                let mut encoded = serde_json::to_string(&response).unwrap();
                encoded.push('\n');
                let _ = writer.write_all(encoded.as_bytes()).await;
                let _ = writer.flush().await;
            }
        }
    });
    let handler = GrokClientHandler {
        info: McpClient::make_client_info("recording", /* advertise_elicitation */ false),
        server_name: "recording".to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(None)),
        elicitation_tx: Arc::new(parking_lot::Mutex::new(None)),
    };
    let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
        client_read,
        client_write,
    );
    let service: McpService = Arc::new(handler.serve(transport).await.expect("handshake"));
    (service, received)
}

fn cancellations(received: &parking_lot::Mutex<Vec<serde_json::Value>>) -> Vec<serde_json::Value> {
    messages_with_method(received, "notifications/cancelled", "/params/requestId")
}

fn call_ids(received: &parking_lot::Mutex<Vec<serde_json::Value>>) -> Vec<serde_json::Value> {
    messages_with_method(received, "tools/call", "/id")
}

/// The `pointer` field of every recorded message whose `method` is `method`.
fn messages_with_method(
    received: &parking_lot::Mutex<Vec<serde_json::Value>>,
    method: &str,
    pointer: &str,
) -> Vec<serde_json::Value> {
    received
        .lock()
        .iter()
        .filter(|m| m.get("method").and_then(|v| v.as_str()) == Some(method))
        .map(|m| {
            m.pointer(pointer)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

#[tokio::test]
async fn dropped_tool_call_sends_one_cancellation_for_its_request() {
    let (service, received) = recording_service(/* reply */ false).await;
    let params = CallToolRequestParams::new("slow");
    let mut call = Box::pin(call_tool_cancel_aware(
        &service,
        params,
        std::time::Duration::from_secs(30),
    ));
    // Let the request go out, then abandon it the way an aborted turn does.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut call)
            .await
            .is_err(),
        "the server never replies, so the call must still be pending"
    );
    drop(call);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let ids = call_ids(&received);
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(
        cancellations(&received),
        ids,
        "one cancel, for the dropped request"
    );
}

#[tokio::test]
async fn timed_out_tool_call_is_classified_and_cancelled_once() {
    let (service, received) = recording_service(/* reply */ false).await;
    let round = call_tool_cancel_aware(
        &service,
        CallToolRequestParams::new("slow"),
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(matches!(round, Err(ServiceError::Timeout { .. })));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let ids = call_ids(&received);
    assert_eq!(
        cancellations(&received),
        ids,
        "rmcp's timeout cancel, and nothing more"
    );
}

#[tokio::test]
async fn completed_tool_call_sends_no_cancellation() {
    let (service, received) = recording_service(/* reply */ true).await;
    let round = call_tool_cancel_aware(
        &service,
        CallToolRequestParams::new("fast"),
        std::time::Duration::from_secs(5),
    )
    .await;
    assert!(matches!(
        round,
        Ok(rmcp::model::CallToolResponse::Complete(_))
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(cancellations(&received).is_empty());
}
