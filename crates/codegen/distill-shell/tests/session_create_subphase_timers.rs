// Modified for Distill by Samuel Fajreldines, 2026.
#[allow(dead_code)]
mod acp_harness;

use std::time::{Duration, Instant};

use acp_harness::{AutoApproveClient, connect_and_auth, new_session, run_agent_test};
use tracing_subscriber::Registry;
use tracing_subscriber::prelude::*;

const SUBPHASE_TIMERS: [&str; 6] = [
    "session.spawn_and_register.session_env",
    "session.spawn_and_register.plugin_refresh",
    "session.spawn_actor.permission_setup",
    "session.spawn_actor.agent_build",
    "session.new_session.git_discovery",
    "session.new_session.tool_overrides_echo",
];

#[test]
fn session_create_emits_a_timer_for_each_serial_subphase() {
    let log_path = std::env::temp_dir().join(format!(
        "session-create-subphase-timers-{}.jsonl",
        std::process::id()
    ));
    // SAFETY: set before any agent code; mode is read once on first use.
    unsafe {
        std::env::set_var("GROK_INSTRUMENTATION", "log");
        std::env::set_var("GROK_INSTRUMENTATION_LOG", &log_path);
    }
    let _ = tracing_subscriber::registry()
        .with(distill_shell::instrumentation::layer::<Registry>())
        .try_init();

    run_agent_test(|cwd, _server| async move {
        let (conn, _init) = connect_and_auth(AutoApproveClient, "subphase-timer-test").await;
        let _session_id = new_session(&conn, &cwd).await;
    });

    let _ = distill_shell::instrumentation::finalize();
    let deadline = Instant::now() + Duration::from_secs(5);
    let log = loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if SUBPHASE_TIMERS.iter().all(|name| log.contains(name)) || Instant::now() >= deadline {
            break log;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    for name in SUBPHASE_TIMERS {
        assert!(
            log.contains(name),
            "session create must emit the {name} sub-timer; log:\n{log}"
        );
    }
    let _ = std::fs::remove_file(&log_path);
}
