// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use agent_client_protocol as acp;
use distill_acp_lib::{AcpAgentGatewaySender, AcpClientMessage};
use distill_paths::AbsPathBuf;
use distill_workspace::permission::types::{
    PatternMode, PermissionConfig, PermissionRule, RuleAction, ToolFilter,
};
use distill_workspace::permission::{
    AccessKind, ClientType, Decision, PermissionCommand, PermissionHandle, PermissionRequest,
    PermissionState, spawn_permission_manager, spawn_permission_manager_with_hub,
};
use serial_test::serial;
use tokio::sync::{mpsc, oneshot};

fn test_home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.keep();
        // SAFETY: called once at init before other threads touch this var.
        unsafe { std::env::set_var("GROK_HOME", &path) };
        path
    })
}

fn fresh_cwd() -> AbsPathBuf {
    let home = test_home();
    let unique = format!(
        "test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cwd = home.join(unique);
    std::fs::create_dir_all(&cwd).unwrap();
    AbsPathBuf::new(cwd).unwrap()
}

fn permission_state_path(cwd: &AbsPathBuf) -> PathBuf {
    test_home()
        .join("sessions")
        .join(urlencoding::encode(cwd.as_str()).into_owned())
        .join("permission.toml")
}

fn load_state(cwd: &AbsPathBuf) -> PermissionState {
    let path = permission_state_path(cwd);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("permission state not yet written at {:?}", path));
    toml::from_str(&contents).unwrap()
}

fn tool_call_update(id: &str, name: &str) -> acp::ToolCallUpdate {
    acp::ToolCallUpdate::new(
        acp::ToolCallId::new(Arc::from(id)),
        acp::ToolCallUpdateFields::new()
            .kind(Some(acp::ToolKind::Other))
            .title(Some(name.to_owned())),
    )
}

fn make_session_id() -> acp::SessionId {
    acp::SessionId::new(Arc::from("test-session"))
}

struct FakeGateway {
    sender: AcpAgentGatewaySender,
    expected: tokio::sync::mpsc::UnboundedSender<(String, Option<serde_json::Value>)>,
}

fn fake_gateway() -> (FakeGateway, tokio::task::JoinHandle<()>) {
    let (gw_tx, mut gw_rx) = mpsc::unbounded_channel::<AcpClientMessage>();
    let (script_tx, mut script_rx) =
        mpsc::unbounded_channel::<(String, Option<serde_json::Value>)>();

    let join = tokio::task::spawn_local(async move {
        while let Some(msg) = gw_rx.recv().await {
            if let AcpClientMessage::RequestPermission(args) = msg {
                let (option_id, meta) = script_rx
                    .recv()
                    .await
                    .expect("test ran out of scripted responses");
                let mut response = acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new(Arc::from(option_id.as_str())),
                    )),
                );
                if let Some(m) = meta.and_then(|v| v.as_object().cloned()) {
                    response = response.meta(m);
                }
                let _ = args.response_tx.send(Ok(response));
            }
        }
    });

    let sender = AcpAgentGatewaySender::new(gw_tx);
    (
        FakeGateway {
            sender,
            expected: script_tx,
        },
        join,
    )
}

impl FakeGateway {
    fn expect_allow_always_mcp_tool(&self, tool_name: &str) {
        let meta = serde_json::json!({
            "kind": "tool",
            "tool_name": tool_name,
        });
        self.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();
    }

    fn expect_allow_always_mcp_server(&self, server: &str) {
        let meta = serde_json::json!({
            "kind": "server",
            "server": server,
        });
        self.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();
    }

    fn expect_plain_allow_always(&self) {
        self.expected
            .send(("always-allow".to_string(), None))
            .unwrap();
    }
}

async fn request(handle: &PermissionHandle, access: AccessKind, id: &str) -> Decision {
    let (tx, rx) = oneshot::channel();
    let cmd = PermissionCommand::Request {
        request: PermissionRequest::new(access, tool_call_update(id, "mcp")),
        respond_to: tx,
    };
    let PermissionHandle::Actor { cmd_tx, .. } = handle else {
        panic!("expected actor handle");
    };
    cmd_tx.send(cmd).unwrap();
    rx.await.unwrap().decision
}

fn mcp(name: &str) -> AccessKind {
    AccessKind::MCPTool {
        name: name.to_string(),
        input: serde_json::Value::Null,
    }
}

async fn run_actor_test<F, Fut>(client_type: ClientType, body: F)
where
    F: FnOnce(PermissionHandle, FakeGateway, AbsPathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_actor_test_with_policy(client_type, None, body).await;
}

async fn run_actor_test_with_policy<F, Fut>(
    client_type: ClientType,
    policy: Option<PermissionConfig>,
    body: F,
) where
    F: FnOnce(PermissionHandle, FakeGateway, AbsPathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_actor_test_full(client_type, policy, false, body).await;
}

async fn run_actor_test_full<F, Fut>(
    client_type: ClientType,
    policy: Option<PermissionConfig>,
    initial_yolo: bool,
    body: F,
) where
    F: FnOnce(PermissionHandle, FakeGateway, AbsPathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let cwd = fresh_cwd();
            let (gw, _gw_task) = fake_gateway();
            let (handle, _events) = spawn_permission_manager(
                make_session_id(),
                gw.sender.clone(),
                cwd.clone(),
                client_type,
                policy,
                vec![],
                vec![],
                initial_yolo,
                None,
            );
            body(handle, gw, cwd).await;
        })
        .await;
}

fn rule(action: RuleAction, pattern: &str) -> PermissionRule {
    PermissionRule {
        action,
        tool: ToolFilter::Mcp,
        pattern: Some(pattern.to_owned()),
        pattern_mode: PatternMode::Glob,
    }
}

#[tokio::test]
#[serial]
async fn mcp_tool_grant_persists_and_short_circuits_next_request() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        gw.expect_allow_always_mcp_tool("linear__list");
        let d = request(&handle, mcp("linear__list"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let state = load_state(&cwd);
        assert!(state.allowed_mcp_tools.contains("linear__list"));
        assert!(state.allowed_mcp_servers.is_empty());

        let d = request(&handle, mcp("linear__list"), "2").await;
        assert!(matches!(d, Decision::Allow));

        gw.expect_allow_always_mcp_server("linear");
        let d = request(&handle, mcp("linear__create"), "3").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            let s = load_state(&cwd);
            if s.allowed_mcp_servers.contains("linear") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(state.allowed_mcp_servers.contains("linear"));

        let d = request(&handle, mcp("linear__update"), "4").await;
        assert!(matches!(d, Decision::Allow));

        gw.expect_allow_always_mcp_tool("notion__fetch");
        let d = request(&handle, mcp("notion__fetch"), "5").await;
        assert!(matches!(d, Decision::Allow));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn fallback_client_plain_allow_always_persists_mcp_tool() {
    run_actor_test(ClientType::Generic, |handle, gw, cwd| async move {
        gw.expect_plain_allow_always();
        let d = request(&handle, mcp("notion__fetch"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let state = load_state(&cwd);
        assert!(
            state.allowed_mcp_tools.contains("notion__fetch"),
            "fallback client AllowAlways must persist tool-scope, got state={state:?}"
        );

        let d = request(&handle, mcp("notion__fetch"), "2").await;
        assert!(matches!(d, Decision::Allow));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn policy_ask_suppresses_mcp_tool_allowlist() {
    let policy = PermissionConfig::new(vec![rule(RuleAction::Ask, "linear__*")]);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let cwd = fresh_cwd();

            let mut state = PermissionState::default();
            state.allowed_mcp_tools.insert("linear__list".to_owned());
            let dir = test_home()
                .join("sessions")
                .join(urlencoding::encode(cwd.as_str()).into_owned());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("permission.toml"),
                toml::to_string_pretty(&state).unwrap(),
            )
            .unwrap();

            let (gw, _gw_task) = fake_gateway();
            let (handle, _events) = spawn_permission_manager_with_hub(
                make_session_id(),
                gw.sender.clone(),
                cwd.clone(),
                ClientType::GrokPager,
                Some(policy),
                vec![],
                vec![],
                false,
                None,
                false,
                None,
            );

            gw.expected.send(("reject-once".to_string(), None)).unwrap();

            let d = request(&handle, mcp("linear__list"), "1").await;
            assert!(matches!(d, Decision::Reject(_)));
        })
        .await;
}

#[tokio::test]
#[serial]
async fn policy_ask_suppresses_mcp_server_allowlist() {
    let policy = PermissionConfig::new(vec![rule(RuleAction::Ask, "linear__*")]);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let cwd = fresh_cwd();

            let mut state = PermissionState::default();
            state.allowed_mcp_servers.insert("linear".to_owned());
            let dir = test_home()
                .join("sessions")
                .join(urlencoding::encode(cwd.as_str()).into_owned());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("permission.toml"),
                toml::to_string_pretty(&state).unwrap(),
            )
            .unwrap();

            let (gw, _gw_task) = fake_gateway();
            let (handle, _events) = spawn_permission_manager_with_hub(
                make_session_id(),
                gw.sender.clone(),
                cwd.clone(),
                ClientType::GrokPager,
                Some(policy),
                vec![],
                vec![],
                false,
                None,
                false,
                None,
            );

            gw.expected.send(("reject-once".to_string(), None)).unwrap();

            let d = request(&handle, mcp("linear__create"), "1").await;
            assert!(matches!(d, Decision::Reject(_)));
        })
        .await;
}

#[tokio::test]
#[serial]
async fn policy_deny_takes_precedence_over_mcp_allowlist() {
    let policy = PermissionConfig::new(vec![rule(RuleAction::Deny, "linear__*")]);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let cwd = fresh_cwd();

            let mut state = PermissionState::default();
            state.allowed_mcp_tools.insert("linear__list".to_owned());
            let dir = test_home()
                .join("sessions")
                .join(urlencoding::encode(cwd.as_str()).into_owned());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("permission.toml"),
                toml::to_string_pretty(&state).unwrap(),
            )
            .unwrap();

            let (gw, _gw_task) = fake_gateway();
            let (handle, _events) = spawn_permission_manager(
                make_session_id(),
                gw.sender.clone(),
                cwd.clone(),
                ClientType::GrokPager,
                Some(policy),
                vec![],
                vec![],
                false,
                None,
            );

            let d = request(&handle, mcp("linear__list"), "1").await;
            assert!(matches!(d, Decision::PolicyDeny(_)));
        })
        .await;
}

#[tokio::test]
#[serial]
async fn policy_allow_short_circuits_before_mcp_allowlist() {
    let policy = PermissionConfig::new(vec![rule(RuleAction::Allow, "linear__*")]);

    run_actor_test_with_policy(
        ClientType::GrokPager,
        Some(policy),
        |handle, _gw, _cwd| async move {
            let d = request(&handle, mcp("linear__list"), "1").await;
            assert!(matches!(d, Decision::Allow));
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn empty_server_prefix_falls_back_to_tool_scope() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        let meta = serde_json::json!({
            "kind": "server",
            "server": "",
        });
        gw.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();

        let d = request(&handle, mcp("linear__list"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(state.allowed_mcp_servers.is_empty());
        assert!(state.allowed_mcp_tools.contains("linear__list"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn allow_always_mcp_tool_ignores_client_supplied_tool_name() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        let meta = serde_json::json!({
            "kind": "tool",
            "tool_name": "notion__fetch",
        });
        gw.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();

        let d = request(&handle, mcp("linear__list"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(
            state.allowed_mcp_tools.contains("linear__list"),
            "must persist the access-kind name"
        );
        assert!(
            !state.allowed_mcp_tools.contains("notion__fetch"),
            "must NOT persist the client-supplied name"
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn allow_always_mcp_server_rejects_mismatched_prefix() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        let meta = serde_json::json!({
            "kind": "server",
            "server": "notion",
        });
        gw.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();

        let d = request(&handle, mcp("linear__list"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(
            state.allowed_mcp_servers.is_empty(),
            "must NOT persist any server-scope grant on mismatch"
        );
        assert!(
            !state.allowed_mcp_servers.contains("notion"),
            "must NOT trust the client-supplied server prefix"
        );
        assert!(
            state.allowed_mcp_tools.contains("linear__list"),
            "must downgrade to tool-scope using the access-kind name"
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn allow_always_mcp_server_persists_canonical_prefix_on_match() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        let meta = serde_json::json!({
            "kind": "server",
            "server": "linear",
        });
        gw.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();

        let d = request(&handle, mcp("linear__list"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(state.allowed_mcp_servers.contains("linear"));
        assert!(state.allowed_mcp_tools.is_empty());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn allow_always_mcp_server_downgrades_when_access_has_no_separator() {
    run_actor_test(ClientType::GrokPager, |handle, gw, cwd| async move {
        let meta = serde_json::json!({
            "kind": "server",
            "server": "linear",
        });
        gw.expected
            .send(("allow-always-mcp".to_string(), Some(meta)))
            .unwrap();

        let d = request(&handle, mcp("standalone"), "1").await;
        assert!(matches!(d, Decision::Allow));

        for _ in 0..50 {
            if permission_state_path(&cwd).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = load_state(&cwd);
        assert!(state.allowed_mcp_servers.is_empty());
        assert!(state.allowed_mcp_tools.contains("standalone"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn dont_ask_policy_denies_without_prompting() {
    use distill_workspace::permission::types::{PermissionConfig, PromptPolicy};

    let mut policy = PermissionConfig::new(vec![]);
    policy.prompt_policy = PromptPolicy::Deny;

    run_actor_test_with_policy(
        ClientType::GrokPager,
        Some(policy),
        |handle, _gw, _cwd| async move {
            let d = request(&handle, mcp("linear__list"), "1").await;
            assert!(matches!(d, Decision::PolicyDeny(_)));

            let d = request(&handle, AccessKind::Bash("npm install".to_string()), "2").await;
            assert!(matches!(d, Decision::PolicyDeny(_)));

            let d = request(
                &handle,
                AccessKind::Read(Some("/tmp/test.txt".to_string())),
                "3",
            )
            .await;
            assert!(matches!(d, Decision::Allow));
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn deny_rule_enforced_in_yolo_mode_bash() {
    let policy = PermissionConfig::new(vec![PermissionRule {
        action: RuleAction::Deny,
        tool: ToolFilter::Bash,
        pattern: Some("rm*".to_owned()),
        pattern_mode: PatternMode::Glob,
    }]);

    run_actor_test_full(
        ClientType::GrokPager,
        Some(policy),
        true,
        |handle, _gw, _cwd| async move {
            let d = request(
                &handle,
                AccessKind::Bash("rm -rf /tmp/foo".to_string()),
                "1",
            )
            .await;
            assert!(
                matches!(d, Decision::PolicyDeny(_)),
                "deny rule must block even in YOLO mode, got {d:?}"
            );

            let d = request(&handle, AccessKind::Bash("cargo test".to_string()), "2").await;
            assert!(
                matches!(d, Decision::Allow),
                "non-denied bash must auto-approve in YOLO mode, got {d:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn deny_rule_enforced_in_yolo_mode_mcp() {
    let policy = PermissionConfig::new(vec![PermissionRule {
        action: RuleAction::Deny,
        tool: ToolFilter::Mcp,
        pattern: Some("dangerous__*".to_owned()),
        pattern_mode: PatternMode::Glob,
    }]);

    run_actor_test_full(
        ClientType::GrokPager,
        Some(policy),
        true,
        |handle, _gw, _cwd| async move {
            let d = request(&handle, mcp("dangerous__delete_all"), "1").await;
            assert!(
                matches!(d, Decision::PolicyDeny(_)),
                "deny rule must block MCP tool even in YOLO mode, got {d:?}"
            );

            let d = request(&handle, mcp("linear__list"), "2").await;
            assert!(
                matches!(d, Decision::Allow),
                "non-denied MCP tool must auto-approve in YOLO mode, got {d:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn deny_rule_enforced_in_yolo_mode_edit() {
    let policy = PermissionConfig::new(vec![PermissionRule {
        action: RuleAction::Deny,
        tool: ToolFilter::Edit,
        pattern: Some("/etc/**".to_owned()),
        pattern_mode: PatternMode::Glob,
    }]);

    run_actor_test_full(
        ClientType::GrokPager,
        Some(policy),
        true,
        |handle, _gw, _cwd| async move {
            let d = request(&handle, AccessKind::Edit("/etc/passwd".to_string()), "1").await;
            assert!(
                matches!(d, Decision::PolicyDeny(_)),
                "deny rule must block edits even in YOLO mode, got {d:?}"
            );

            let d = request(&handle, AccessKind::Edit("src/main.rs".to_string()), "2").await;
            assert!(
                matches!(d, Decision::Allow),
                "non-denied edit must auto-approve in YOLO mode, got {d:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn deny_rule_enforced_in_yolo_mode_web_fetch() {
    let policy = PermissionConfig::new(vec![PermissionRule {
        action: RuleAction::Deny,
        tool: ToolFilter::WebFetch,
        pattern: Some("evil.com".to_owned()),
        pattern_mode: PatternMode::Domain,
    }]);

    run_actor_test_full(
        ClientType::GrokPager,
        Some(policy),
        true,
        |handle, _gw, _cwd| async move {
            let d = request(
                &handle,
                AccessKind::WebFetch("https://evil.com/exfiltrate".to_string()),
                "1",
            )
            .await;
            assert!(
                matches!(d, Decision::PolicyDeny(_)),
                "deny rule must block web_fetch even in YOLO mode, got {d:?}"
            );

            let d = request(
                &handle,
                AccessKind::WebFetch("https://docs.rs/tokio".to_string()),
                "2",
            )
            .await;
            assert!(
                matches!(d, Decision::Allow),
                "non-denied web_fetch must auto-approve in YOLO mode, got {d:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn yolo_mode_without_deny_rules_approves_everything() {
    run_actor_test_full(
        ClientType::GrokPager,
        None,
        true,
        |handle, _gw, _cwd| async move {
            let d = request(&handle, AccessKind::Bash("rm -rf /".to_string()), "1").await;
            assert!(matches!(d, Decision::Allow));

            let d = request(&handle, mcp("linear__list"), "2").await;
            assert!(matches!(d, Decision::Allow));

            let d = request(&handle, AccessKind::Edit("/etc/passwd".to_string()), "3").await;
            assert!(matches!(d, Decision::Allow));

            let d = request(
                &handle,
                AccessKind::WebFetch("https://evil.com".to_string()),
                "4",
            )
            .await;
            assert!(matches!(d, Decision::Allow));
        },
    )
    .await;
}
