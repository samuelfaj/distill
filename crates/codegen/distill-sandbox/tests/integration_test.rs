// Modified for Distill by Samuel Fajreldines, 2026.
//! `Sandbox::apply()` is irreversible and process-wide, so standard `#[test]` functions (which share a process) cannot test kernel enforcement.
//! Use the `sandbox_smoke_test` example and the e2e binaries for enforcement testing.
//! These tests verify public API contracts that do not require a live kernel sandbox.

// `to_capability_set` is only available with the `enforce` feature.
#[test]
#[cfg(all(feature = "enforce", unix))]
fn test_profile_capability_set_construction() {
    use distill_sandbox::ProfileName;

    // CWD is guaranteed to exist
    let workspace = std::env::current_dir().expect("cwd");

    for profile in [
        ProfileName::Workspace,
        ProfileName::ReadOnly,
        ProfileName::Strict,
        ProfileName::Off,
    ] {
        let result = profile.to_capability_set(&workspace);
        assert!(
            result.is_ok(),
            "Profile {:?} failed to build CapabilitySet: {:?}",
            profile,
            result.err()
        );
    }
}

#[test]
fn test_sandbox_manager_lifecycle() {
    use distill_sandbox::{ProfileName, SandboxManager};

    let workspace = std::env::current_dir().expect("cwd");

    // Off profile: apply should succeed without actually sandboxing
    let mut manager = SandboxManager::new(ProfileName::Off, &workspace);
    assert!(!manager.is_applied());
    assert!(!manager.restrict_child_network());

    let result = manager.apply(&workspace);
    assert!(result.is_ok());
    assert!(!manager.is_applied());
}

#[test]
fn test_sandbox_logger() {
    use distill_sandbox::{SandboxEvent, SandboxLogger};

    let logger = SandboxLogger::new();

    // Log violation events; profile_applied requires a resolved profile
    logger.log(SandboxEvent::fs_violation("workspace", "/tmp/test", "read"));
    logger.log(SandboxEvent::fs_violation(
        "workspace",
        "/etc/shadow",
        "write",
    ));
    logger.log(SandboxEvent::net_violation("strict", "evil.com:443"));

    // Check metrics
    assert_eq!(logger.metrics().fs_violation_count(), 2);
    assert_eq!(logger.metrics().net_violation_count(), 1);

    // Take events drains the buffer
    let events = logger.take_events();
    assert_eq!(events.len(), 3);

    // Buffer is now empty
    let events2 = logger.take_events();
    assert!(events2.is_empty());
}

#[test]
fn test_should_restrict_child_network_default() {
    // The "set" path needs an applied sandbox, which is irreversible, so only the default (false) is verifiable here
    // The global is set once at process startup and never unset; a test that applies a sandbox would interfere with this one
    assert!(!distill_sandbox::should_restrict_child_network());
}
