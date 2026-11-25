// Modified for Distill by Samuel Fajreldines, 2026.
//! `GROK_EXTRA_HOOKS_FILE` lets the host process inject a hooks file next to discovered hooks.
//!
//! Env vars are process-global, so everything runs in one test of this binary, phase by phase.

use distill_hooks::discovery::{EXTRA_HOOKS_FILE_ENV, load_hooks};
use distill_hooks::dispatcher;
use distill_hooks::event::*;
use distill_hooks::result::HookDecision;
use distill_hooks::runner::RunContext;

fn pre_tool_use_envelope(tool_name: &str) -> HookEventEnvelope {
    HookEventEnvelope {
        hook_event_name: HookEventName::PreToolUse,
        session_id: "test-session".into(),
        cwd: "/tmp".into(),
        workspace_root: "/tmp".into(),
        timestamp: "2025-01-01T00:00:00Z".into(),
        transcript_path: None,
        client_identifier: None,
        prompt_id: None,
        permission_mode: None,
        payload: HookPayload::PreToolUse {
            tool_name: tool_name.into(),
            tool_use_id: "call-1".into(),
            tool_input: serde_json::json!({"file_path": "/tmp/a.txt"}),
            tool_input_truncated: false,
            subagent_type: None,
        },
    }
}

async fn decision_for(
    registry: &distill_hooks::discovery::HookRegistry,
    tool: &str,
) -> HookDecision {
    let ctx = RunContext {
        session_id: "test",
        workspace_root: "/tmp",
        process_scope: None,
        disabled: Default::default(),
    };
    dispatcher::dispatch_pre_tool_use(registry, &pre_tool_use_envelope(tool), &ctx)
        .await
        .decision
}

#[tokio::test]
async fn extra_hooks_file_loads_beside_discovered_hooks_and_can_deny_edit_tools() {
    let dir = tempfile::tempdir().unwrap();
    let discovered = dir.path().join("discovered");
    std::fs::create_dir(&discovered).unwrap();
    std::fs::write(
        discovered.join("own.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
    )
    .unwrap();
    let extra = dir.path().join("host-hooks.json");
    std::fs::write(
        &extra,
        r#"{"hooks":{"PreToolUse":[{"matcher":"write|search_replace|apply_patch","hooks":[{"type":"command","command":"echo '{\"decision\":\"deny\",\"reason\":\"file leased by another run\"}'; exit 2"}]}]}}"#,
    )
    .unwrap();

    // Without the env var the host hook does not exist.
    unsafe { std::env::remove_var(EXTRA_HOOKS_FILE_ENV) };
    let (registry, errors) = load_hooks(Some(&discovered), None);
    assert!(errors.is_empty(), "errors: {errors:?}");
    assert_eq!(registry.len(), 1);
    assert_eq!(
        decision_for(&registry, "search_replace").await,
        HookDecision::Allow
    );

    // With the env var the extra PreToolUse hook is added to the discovered one and denies edit tools.
    unsafe { std::env::set_var(EXTRA_HOOKS_FILE_ENV, &extra) };
    let (registry, errors) = load_hooks(Some(&discovered), None);
    assert!(errors.is_empty(), "errors: {errors:?}");
    assert_eq!(registry.len(), 2, "discovered + injected hooks");
    for tool in ["search_replace", "write", "apply_patch"] {
        match decision_for(&registry, tool).await {
            HookDecision::Deny { reason, .. } => {
                assert!(reason.contains("leased"), "{tool}: reason {reason}")
            }
            other => panic!("{tool}: expected Deny, got {other:?}"),
        }
    }
    assert_eq!(
        decision_for(&registry, "read_file").await,
        HookDecision::Allow,
        "matcher must not deny non-edit tools"
    );

    // A missing file warns and the session continues with the discovered hooks.
    unsafe { std::env::set_var(EXTRA_HOOKS_FILE_ENV, dir.path().join("missing.json")) };
    let (registry, errors) = load_hooks(Some(&discovered), None);
    assert_eq!(errors.len(), 1, "missing file is reported: {errors:?}");
    assert_eq!(registry.len(), 1);

    // An invalid file warns and the session continues with the discovered hooks.
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    unsafe { std::env::set_var(EXTRA_HOOKS_FILE_ENV, &bad) };
    let (registry, errors) = load_hooks(Some(&discovered), None);
    assert_eq!(errors.len(), 1, "invalid file is reported: {errors:?}");
    assert_eq!(registry.len(), 1);

    // A relative path is refused rather than resolved against an unknown cwd.
    unsafe { std::env::set_var(EXTRA_HOOKS_FILE_ENV, "host-hooks.json") };
    let (registry, errors) = load_hooks(Some(&discovered), None);
    assert_eq!(errors.len(), 1, "relative path is reported: {errors:?}");
    assert_eq!(registry.len(), 1);

    unsafe { std::env::remove_var(EXTRA_HOOKS_FILE_ENV) };
}
