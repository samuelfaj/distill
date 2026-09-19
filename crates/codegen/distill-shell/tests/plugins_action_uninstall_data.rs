// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: the pager modal Uninstall (`x.ai/plugins/action`) must clean up
//! `~/.grok/plugin-data/<id>/` like the CLI uninstall path, not orphan it.

mod acp_harness;

use acp_harness::{AutoApproveClient, connect_and_auth, ext_method, new_session};
use serde_json::json;

fn action_outcome(response: &serde_json::Value) -> distill_hooks_plugins_types::ActionOutcome {
    let inner = response.get("result").unwrap_or(response);
    serde_json::from_value(inner.clone())
        .unwrap_or_else(|e| panic!("bad ActionOutcome ({e}): {response}"))
}

#[test]
fn plugins_action_uninstall_removes_plugin_data_dir() {
    acp_harness::run_agent_test(|cwd, _server| async move {
        let distill_home =
            std::path::PathBuf::from(std::env::var("GROK_HOME").expect("harness sets GROK_HOME"));

        let plugin_dir = cwd.join("data-demo");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("plugin.json"), r#"{"name":"data-demo"}"#).unwrap();

        let (conn, _init) = connect_and_auth(AutoApproveClient, "uninstall-data-test").await;
        let session_id = new_session(&conn, &cwd).await;

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
            distill_hooks_plugins_types::OutcomeStatus::Success,
            "install must succeed: {}",
            outcome.message
        );

        // Simulate persisted plugin state at the id the uninstall must clean,
        // derived exactly like `git_install::cleanup_plugin_data`.
        use distill_agent::plugins::discovery::{PluginId, PluginScope};
        use distill_agent::plugins::install_registry::InstallRegistry;
        let registry = InstallRegistry::load();
        let (_, repo, _) = registry.find_plugin("data-demo").expect("repo registered");
        let scope = match distill_dirs::home_dir() {
            Some(home) if repo.path.starts_with(&home) => PluginScope::User,
            _ => PluginScope::ConfigPath,
        };
        let id = PluginId::new(scope, &repo.path, "data-demo");
        let data_dir = distill_home.join("plugin-data").join(&id.0);
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("state.json"), "{}").unwrap();
        let repo_path = repo.path.clone();
        drop(registry);

        let response = ext_method(
            &conn,
            "x.ai/plugins/action",
            json!({
                "sessionId": session_id.0.to_string(),
                "action": {"type": "uninstall", "plugin_id": "data-demo", "confirmed": false},
            }),
        )
        .await;
        let outcome = action_outcome(&response);
        assert_eq!(
            outcome.status,
            distill_hooks_plugins_types::OutcomeStatus::Success,
            "uninstall must succeed: {}",
            outcome.message
        );

        assert!(!repo_path.exists(), "repo dir must be removed");
        assert!(
            InstallRegistry::load().find_plugin("data-demo").is_none(),
            "registry entry must be removed"
        );
        assert!(
            !data_dir.exists(),
            "plugin-data dir must be cleaned up like the CLI uninstall path: {}",
            data_dir.display()
        );
    });
}
