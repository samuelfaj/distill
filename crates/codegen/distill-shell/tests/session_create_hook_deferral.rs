// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg(unix)]

#[allow(dead_code)]
mod acp_harness;

use std::time::Duration;

use acp_harness::{AutoApproveClient, connect_and_auth, new_session, prompt_turn, run_agent_test};

#[test]
fn session_create_does_not_wait_for_session_start_hooks() {
    run_agent_test(|cwd, _server| async move {
        let distill_home = std::path::PathBuf::from(std::env::var("GROK_HOME").expect("GROK_HOME"));
        let release = distill_home.join("session_start_hook_release");
        let done = distill_home.join("session_start_hook_done");
        let hooks_dir = distill_home.join("hooks");
        std::fs::create_dir_all(&hooks_dir).expect("create hooks dir");
        std::fs::write(
            hooks_dir.join("session_start.json"),
            serde_json::json!({
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{
                            "type": "command",
                            "command": format!(
                                "until [ -e {} ]; do sleep 0.1; done; touch {}",
                                release.display(),
                                done.display()
                            ),
                            "timeout": 120
                        }]
                    }]
                }
            })
            .to_string(),
        )
        .expect("write hook config");

        let (conn, _init) = connect_and_auth(AutoApproveClient, "hook-deferral-test").await;
        let session_id = new_session(&conn, &cwd).await;
        // Release is written after this turn; a blocking create would time out here.
        prompt_turn(&conn, &session_id, "hello").await;
        std::fs::write(&release, "").expect("release the hook");

        let hook_ran = tokio::time::timeout(Duration::from_secs(15), async {
            while !done.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        assert!(hook_ran, "deferred session-start hook must still run");
    });
}
