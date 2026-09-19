// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: managed policy must bind the pager wire paths — plugins/action install/update of
//! unlisted sources refuse, and mcp/toggle of a pin-dropped server cites the org policy.

mod acp_harness;

use std::sync::Arc;

use acp_harness::{
    AutoApproveClient, RPC_TIMEOUT, connect_and_auth, ext_method, new_session, run_agent_test,
};
use agent_client_protocol::{self as acp, Agent as _};
use serde_json::json;

fn action_outcome(response: &serde_json::Value) -> distill_hooks_plugins_types::ActionOutcome {
    let inner = response.get("result").unwrap_or(response);
    serde_json::from_value(inner.clone())
        .unwrap_or_else(|e| panic!("bad ActionOutcome ({e}): {response}"))
}

#[test]
fn plugins_action_install_and_update_respect_marketplace_lockdown() {
    run_agent_test(|cwd, _server| async move {
        let distill_home =
            std::path::PathBuf::from(std::env::var("GROK_HOME").expect("harness sets GROK_HOME"));
        std::fs::create_dir_all(&distill_home).unwrap();
        // Binding lockdown: one allowed marketplace (installs/updates of
        // anything else refuse) and project MCP pinned off.
        std::fs::write(
            distill_home.join("requirements.toml"),
            "enable_all_project_mcp_servers = false\n\n\
             [[strict_known_marketplaces]]\n\
             source = \"git\"\n\
             url = \"https://github.com/allowed/marketplace\"\n",
        )
        .unwrap();

        // Local plugin directory: not on the strict list, so installing it is
        // a lockdown bypass if it succeeds.
        let plugin_dir = cwd.join("unlisted-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"unlisted-plugin"}"#,
        )
        .unwrap();

        // Guard: the fixture must actually arm the policy (in-process agent, same OnceLock) — a
        // vacuous policy would make every refusal assertion below meaningless.
        let ms = distill_workspace::permission::resolution::managed_settings();
        assert!(
            ms.marketplace_allowlist
                .add_block_reason(&plugin_dir.display().to_string())
                .is_some(),
            "strict_known_marketplaces from $GROK_HOME/requirements.toml must load"
        );
        assert!(
            ms.project_mcp.is_disabled(),
            "enable_all_project_mcp_servers pin from requirements.toml must load"
        );

        // Pre-seeded direct git install from an unlisted URL; the checkout deliberately isn't a git
        // repo so a gate miss fails on git, not the network.
        use distill_agent::plugins::install_registry::{
            InstallKind, InstallRegistry, InstalledRepo, RepoPlugin,
        };
        let blocked_repo_dir = distill_home.join("plugins").join("blocked-repo");
        std::fs::create_dir_all(&blocked_repo_dir).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut registry = InstallRegistry::load();
        registry.insert(
            "github.com-blocked/deadbeef/repo".to_string(),
            InstalledRepo {
                kind: InstallKind::Git {
                    url: "https://github.com/blocked/repo.git".to_string(),
                    git_ref: None,
                    commit: "deadbeef".to_string(),
                    subdir: None,
                },
                installed_at: now.clone(),
                updated_at: now,
                path: blocked_repo_dir,
                plugins: std::collections::HashMap::from([(
                    "blocked-plugin".to_string(),
                    RepoPlugin {
                        subdir: None,
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );
        registry.save().expect("seed install registry");

        let (conn, _init) = connect_and_auth(AutoApproveClient, "policy-gate-test").await;
        let session_id = new_session(&conn, &cwd).await;

        // Install action must be refused by the acquisition gate.
        let response = ext_method(
            &conn,
            "x.ai/plugins/action",
            json!({
                "sessionId": session_id.0.to_string(),
                "action": {"type": "install", "source": plugin_dir.display().to_string()},
            }),
        )
        .await;
        let outcome = action_outcome(&response);
        assert_eq!(
            outcome.status,
            distill_hooks_plugins_types::OutcomeStatus::ValidationError,
            "a policy refusal is a validation error, not an internal one: {}",
            outcome.message
        );
        assert!(
            outcome.message.starts_with("Plugin install blocked"),
            "refusal must surface the bare policy message like every sibling \
             surface, got: {}",
            outcome.message
        );
        let registry = InstallRegistry::load();
        assert!(
            registry
                .list()
                .iter()
                .all(|(key, _)| *key == "github.com-blocked/deadbeef/repo"),
            "refused install must not register a repo: {:?}",
            registry.list().iter().map(|(k, _)| k).collect::<Vec<_>>()
        );

        // Update action on the blocked-URL repo must be refused before any
        // fetch (per-repo failure line, not a git error).
        let response = ext_method(
            &conn,
            "x.ai/plugins/action",
            json!({
                "sessionId": session_id.0.to_string(),
                "action": {"type": "update", "plugin_id": "blocked-plugin"},
            }),
        )
        .await;
        let outcome = action_outcome(&response);
        assert!(
            outcome.message.contains("Plugin update blocked"),
            "update refusal must name the policy gate, got: {}",
            outcome.message
        );
        assert!(
            !outcome.requires_reload,
            "a refused update must not request a reload"
        );

        // Pin-dropped project server: the enable refusal must cite the
        // organization policy, not claim the server is missing from config.
        std::fs::create_dir_all(cwd.join(".cursor")).unwrap();
        std::fs::write(
            cwd.join(".cursor").join("mcp.json"),
            r#"{"mcpServers": {"projsrv": {"url": "https://proj.example.test/mcp"}}}"#,
        )
        .unwrap();
        let params = serde_json::value::RawValue::from_string(
            json!({
                "session_id": session_id.0.to_string(),
                "server_name": "projsrv",
                "enabled": true,
            })
            .to_string(),
        )
        .expect("serialize mcp/toggle params");
        let err = tokio::time::timeout(
            RPC_TIMEOUT,
            conn.ext_method(acp::ExtRequest::new("x.ai/mcp/toggle", Arc::from(params))),
        )
        .await
        .expect("mcp/toggle timed out")
        .expect_err("enable of a pin-dropped project server must refuse");
        let detail = format!("{err:?}");
        assert!(
            detail.contains("organization policy"),
            "refusal must cite the pinning policy, got: {detail}"
        );
    });
}
