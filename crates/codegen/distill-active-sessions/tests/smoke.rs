// Modified for Distill by Samuel Fajreldines, 2026.
//! Runs the full session flow in one temp directory: register, clean unregister, then crash detection.
//! Proves a clean exit removes its entry and crash detection returns only the session with a dead PID.

use chrono::Utc;
use tempfile::TempDir;
use distill_active_sessions::*;

fn session(id: &str, pid: u32) -> ActiveSession {
    ActiveSession {
        session_id: agent_client_protocol::SessionId::new(id),
        pid,
        cwd: "/tmp/test".into(),
        opened_at: Utc::now(),
    }
}

#[test]
fn full_lifecycle() {
    let dir = TempDir::new().unwrap();
    let r = dir.path();
    let pid = std::process::id();
    let sid = |s: &str| agent_client_protocol::SessionId::new(s);

    // Start a session and verify it is listed
    register_in(r, session("s1", pid)).unwrap();
    assert_eq!(list_in(r).unwrap().len(), 1);

    // Unregister (a clean exit) and verify the entry is gone
    unregister_in(r, &sid("s1")).unwrap();
    assert!(list_in(r).unwrap().is_empty());

    // Register a crashed session (dead PID) alongside a live one
    register_in(r, session("crashed", 2_000_000_000)).unwrap();
    register_in(r, session("alive", pid)).unwrap();

    // Crash detection returns the dead session and keeps the live one
    let crashed = collect_crashed_in(r).unwrap();
    assert_eq!(crashed.len(), 1);
    assert_eq!(&*crashed[0].session_id.0, "crashed");
    assert_eq!(list_in(r).unwrap().len(), 1);
}
