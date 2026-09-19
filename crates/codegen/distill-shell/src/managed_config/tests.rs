// Modified for Distill by Samuel Fajreldines, 2026.
use super::policy::*;
use super::response::ManagedConfigResponse;
use super::store::*;
use super::supervisor::*;
use super::*;

#[test]
fn gate_snapshot_denies_when_lock_held_or_unopenable() {
    let short_wait = std::time::Duration::from_millis(150);
    let dir = tempfile::tempdir().unwrap();
    let _held = try_lock_managed_config(dir.path()).expect("test takes the lock first");
    assert_eq!(
        locked_gate_snapshot(dir.path(), short_wait).map(|_| ()),
        Err(ManagedPolicyRefusal::Busy),
        "a contended gate lock must fail closed as busy"
    );
    let missing = dir.path().join("missing/home");
    assert_eq!(
        locked_gate_snapshot(&missing, short_wait).map(|_| ()),
        Err(ManagedPolicyRefusal::LockUnavailable {
            home: missing.clone()
        }),
        "an unopenable lock file must fail closed as unavailable, not busy"
    );
}

#[test]
fn gate_waits_out_a_contended_lock_inside_a_local_set_on_a_multi_thread_runtime() {
    let short_wait = std::time::Duration::from_millis(150);
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let held = try_lock_managed_config(dir.path()).expect("test takes the lock first");
    let home = dir.path().to_path_buf();
    let started = std::time::Instant::now();
    let busy = local.block_on(&runtime, async move {
        locked_gate_snapshot(&home, short_wait).map(|_| ())
    });
    let waited = started.elapsed();
    assert_eq!(
        busy,
        Err(ManagedPolicyRefusal::Busy),
        "a lock held for the whole wait must fail closed as busy, not panic"
    );
    // Pins the bounded-poll contract: a regression to unbounded blocking must fail here, not hang as a CI timeout.
    assert!(
        waited < std::time::Duration::from_secs(1),
        "the contended wait must stay near lock_wait ({short_wait:?}), took {waited:?}"
    );

    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });
    let home = dir.path().to_path_buf();
    let acquired = local.block_on(&runtime, async move {
        locked_gate_snapshot(&home, std::time::Duration::from_secs(5)).map(|_| ())
    });
    release.join().unwrap();
    assert_eq!(
        acquired,
        Ok(()),
        "the gate must take the lock once the holder releases it"
    );
}

#[tokio::test]
async fn refresher_drop_stops_work_without_cancelling_parent() {
    let parent = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    drop(ManagedConfigRefresher::spawn(&parent, async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tx.send(());
    }));
    assert!(
        rx.await.is_err(),
        "the aborted work must never reach its send"
    );
    assert!(!parent.is_cancelled());
}

#[tokio::test]
async fn supervisor_slot_respawns_a_dead_task_and_keeps_a_live_one() {
    let dead = ManagedConfigRefresher::spawn(&tokio_util::sync::CancellationToken::new(), async {});
    while !dead.handle.is_finished() {
        tokio::task::yield_now().await;
    }
    let dead_token = dead.cancel.clone();
    *REFRESH_SUPERVISOR.lock().unwrap() = Some(dead);

    ensure_supervisor(|| {
        ManagedConfigRefresher::spawn(
            &tokio_util::sync::CancellationToken::new(),
            std::future::pending(),
        )
    });
    assert!(
        dead_token.is_cancelled(),
        "the finished supervisor must be replaced (its guard dropped)"
    );
    {
        let slot = REFRESH_SUPERVISOR.lock().unwrap();
        assert!(
            !slot.as_ref().unwrap().handle.is_finished(),
            "a live supervisor must now occupy the slot"
        );
    }

    let live_token = REFRESH_SUPERVISOR
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .cancel
        .clone();
    ensure_supervisor(|| unreachable!("a live supervisor must be kept, not respawned"));
    assert!(!live_token.is_cancelled());

    *REFRESH_SUPERVISOR.lock().unwrap() = None;
}

#[test]
fn gate_blocks_only_managed_principal_with_compromised_policy() {
    let snapshot = |managed_principal_present, policy_compromised| GateSnapshot {
        managed_principal_present,
        policy_compromised,
    };
    assert!(
        managed_policy_gate_decision(snapshot(true, true)).is_err(),
        "managed principal + compromised policy must fail closed"
    );
    assert!(managed_policy_gate_decision(snapshot(true, false)).is_ok());
    assert!(managed_policy_gate_decision(snapshot(false, true)).is_ok());
    assert!(managed_policy_gate_decision(snapshot(false, false)).is_ok());
}

#[test]
fn apply_writes_and_overwrites_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::write(home.join("managed_config.toml"), "[cli]\nold = true\n").unwrap();

    let body = ManagedConfigResponse {
        deployment_id: None,
        team_id: None,
        managed_config: Some("[cli]\ntheme = \"dark\"\n".into()),
        requirements: Some("[features]\nweb_fetch = false\n".into()),
        ..Default::default()
    };
    assert!(apply_managed_config(home, &body).unwrap());

    assert_eq!(
        std::fs::read_to_string(home.join("managed_config.toml")).unwrap(),
        "[cli]\ntheme = \"dark\"\n",
        "managed_config is overwritten with the served content"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("requirements.toml")).unwrap(),
        "[features]\nweb_fetch = false\n"
    );
}

#[test]
fn apply_removes_artifact_the_server_no_longer_serves() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();

    let body = ManagedConfigResponse {
        deployment_id: None,
        team_id: None,
        managed_config: Some("[cli]\ntheme = \"dark\"\n".into()),
        requirements: None,
        ..Default::default()
    };
    assert!(apply_managed_config(home, &body).unwrap());

    assert!(home.join("managed_config.toml").exists());
    assert!(
        !home.join("requirements.toml").exists(),
        "an artifact the server no longer serves is removed"
    );

    let withdrawn = ManagedConfigResponse {
        deployment_id: None,
        team_id: None,
        managed_config: Some(String::new()),
        requirements: None,
        ..Default::default()
    };
    assert!(apply_managed_config(home, &withdrawn).unwrap());
    assert!(
        !home.join("managed_config.toml").exists(),
        "empty served content converges to absence"
    );

    assert!(!apply_managed_config(home, &withdrawn).unwrap());
}

#[cfg(unix)]
#[test]
fn apply_partial_write_failure_keeps_written_artifact() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // A squat whose child can't be unlinked: the squat-clear and the rename both fail.
    let req = home.join("requirements.toml");
    std::fs::create_dir(&req).unwrap();
    std::fs::write(req.join("pin"), "x").unwrap();
    std::fs::set_permissions(&req, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::remove_dir_all(&req).is_ok() {
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }

    let body = ManagedConfigResponse {
        deployment_id: None,
        team_id: None,
        managed_config: Some("[cli]\ninstaller = \"internal\"\n".into()),
        requirements: Some("[features]\nweb_fetch = false\n".into()),
        ..Default::default()
    };
    let result = apply_managed_config(home, &body);

    assert!(result.is_err(), "requirements write must fail");
    assert!(
        home.join("managed_config.toml").exists(),
        "the artifact that wrote successfully must be kept"
    );
    let _ = std::fs::set_permissions(&req, std::fs::Permissions::from_mode(0o700));
}

#[test]
fn apply_converges_over_a_squatting_directory() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir(home.join("requirements.toml")).unwrap();
    std::fs::write(home.join("requirements.toml").join("junk"), "x").unwrap();
    std::fs::create_dir(home.join("managed_config.toml")).unwrap();

    let body = ManagedConfigResponse {
        deployment_id: None,
        team_id: None,
        managed_config: Some("[cli]\ntheme = \"dark\"\n".into()),
        requirements: None,
        ..Default::default()
    };
    assert!(apply_managed_config(home, &body).unwrap());
    assert_eq!(
        std::fs::read_to_string(home.join("managed_config.toml")).unwrap(),
        "[cli]\ntheme = \"dark\"\n",
        "the squatting directory is replaced by the served file"
    );
    assert!(
        !home.join("requirements.toml").exists(),
        "the squatting directory in a served-absent slot is removed"
    );
}

#[test]
fn remove_managed_config_files_tolerates_partial_existence() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();
    std::fs::create_dir(home.join("managed_config.toml")).unwrap();
    std::fs::write(home.join("managed_config.toml").join("junk"), "x").unwrap();

    remove_managed_config_files(home);

    for f in [
        "requirements.toml",
        "managed_config.toml",
        "managed_config_cache.json",
        "managed_config.sig.json",
    ] {
        assert!(
            !home.join(f).exists(),
            "{f} must be gone after the purge (absent ones tolerated, dir squat removed)"
        );
    }
}

#[test]
fn staging_classifier_matches_only_the_write_deny_class() {
    use std::io::{Error, ErrorKind};
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::ReadOnlyFilesystem,
        ErrorKind::ResourceBusy,
    ] {
        assert!(
            write_failure_is_deny(&Error::from(kind)),
            "{kind:?} is the sandbox deny class and may stage"
        );
    }
    for kind in [
        ErrorKind::IsADirectory,
        ErrorKind::DirectoryNotEmpty,
        ErrorKind::StorageFull,
        ErrorKind::NotFound,
        ErrorKind::Other,
    ] {
        assert!(
            !write_failure_is_deny(&Error::from(kind)),
            "{kind:?} must propagate loudly, not stage"
        );
    }
}

#[test]
fn transport_failure_maps_to_managed_config_error() {
    use crate::http::{TransportFailure, TransportFailureKind};

    let unreachable = map_transport_failure(TransportFailure {
        kind: TransportFailureKind::Unreachable,
        detail: "connection refused".into(),
    });
    assert!(matches!(unreachable, ManagedConfigError::Network(_)));
    assert!(
        unreachable.is_retryable(),
        "an unreachable server is retried"
    );
    assert!(!unreachable.is_auth_rejection());

    let interrupted = map_transport_failure(TransportFailure {
        kind: TransportFailureKind::Interrupted,
        detail: "connection closed before message completed".into(),
    });
    assert!(matches!(
        interrupted,
        ManagedConfigError::ConnectionInterrupted(_)
    ));
    assert!(
        interrupted.is_retryable(),
        "an in-flight interruption is retried"
    );
    assert!(
        !interrupted.is_auth_rejection(),
        "a transport error is not an auth rejection"
    );

    let permanent = map_transport_failure(TransportFailure {
        kind: TransportFailureKind::Permanent,
        detail: "too many redirects".into(),
    });
    assert!(
        matches!(permanent, ManagedConfigError::RequestFailed(_)),
        "a client-side defect maps to RequestFailed, not InvalidResponse"
    );
    assert!(
        !permanent.is_retryable(),
        "a client-side defect is terminal and must not be retried"
    );
    assert!(!permanent.is_auth_rejection());

    let untrusted = map_transport_failure(TransportFailure {
        kind: TransportFailureKind::CertificateUntrusted,
        detail: "invalid peer certificate: UnknownIssuer".into(),
    });
    assert!(matches!(
        untrusted,
        ManagedConfigError::CertificateUntrusted(_)
    ));
    assert!(
        !untrusted.is_retryable(),
        "the same untrusted certificate will fail again until roots are installed"
    );

    let invalid = map_transport_failure(TransportFailure {
        kind: TransportFailureKind::CertificateInvalid,
        detail: "invalid peer certificate: Expired".into(),
    });
    assert!(matches!(invalid, ManagedConfigError::CertificateInvalid(_)));
    assert!(
        !invalid.is_retryable(),
        "an expired or wrong-host certificate will fail again; retrying cannot fix it"
    );
}

#[test]
fn certificate_detail_names_the_bundle_env_only_when_set() {
    assert_eq!(
        certificate_detail("UnknownIssuer".into(), Some("GROK_EXTRA_CA_BUNDLE"), 2),
        "UnknownIssuer; GROK_EXTRA_CA_BUNDLE is set: verify it includes the issuing root CA"
    );
    assert_eq!(
        certificate_detail("UnknownIssuer".into(), Some("GROK_EXTRA_CA_BUNDLE"), 0),
        "UnknownIssuer; GROK_EXTRA_CA_BUNDLE is set but no usable roots were loaded from it: check that the file is readable, contains PEM certificates, and is under the size cap"
    );
    assert_eq!(
        certificate_detail("UnknownIssuer".into(), None, 0),
        "UnknownIssuer"
    );
}

#[cfg(unix)]
#[test]
fn purge_keeps_marker_when_an_artifact_removal_fails() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for name in MANAGED_ARTIFACT_FILES {
        std::fs::write(home.join(name), "x").unwrap();
    }
    std::fs::write(home.join(distill_config::MANAGED_CONFIG_CACHE_FILE), "{}").unwrap();

    // Unremovable squat: `remove_dir_all` can't unlink inside the read-only subdir.
    let squat = home.join("requirements.toml");
    std::fs::remove_file(&squat).unwrap();
    let locked_subdir = squat.join("locked");
    std::fs::create_dir_all(&locked_subdir).unwrap();
    std::fs::write(locked_subdir.join("pin"), "x").unwrap();
    let readonly = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(&locked_subdir, readonly).unwrap();
    if std::fs::remove_dir_all(&squat).is_ok() {
        // The fault can't be injected: read-only perms don't block removal (root/CI edge).
        eprintln!("skipping: permissions not enforced (running as root?)");
        return;
    }

    remove_managed_config_files(home);
    assert!(
        home.join(distill_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "a failed artifact removal must keep the marker (detector stays armed)"
    );

    std::fs::set_permissions(&locked_subdir, std::fs::Permissions::from_mode(0o755)).unwrap();
    remove_managed_config_files(home);
    for name in MANAGED_ARTIFACT_FILES {
        assert!(!home.join(name).exists(), "{name} must be purged");
    }
    assert!(
        !home
            .join(distill_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "with every artifact removed, the marker goes last"
    );
}

#[test]
fn served_principal_prefers_deployment_id() {
    use distill_config::signed_policy::SignedPayload;
    let payload = |dep: Option<&str>, team: Option<&str>| SignedPayload {
        typ: distill_config::signed_policy::MANAGED_POLICY_TYP.into(),
        version: 1,
        deployment_id: dep.map(Into::into),
        team_id: team.map(Into::into),
        managed_config: None,
        requirements: None,
        fail_closed: false,
        expires_at: 0,
        nonce: String::new(),
        key_id: "v1".into(),
    };
    assert_eq!(
        served_principal_of(&payload(Some("dep-1"), Some("team-007"))),
        Some("dep-1")
    );
    assert_eq!(
        served_principal_of(&payload(None, Some("team-007"))),
        Some("team-007")
    );
    assert_eq!(served_principal_of(&payload(None, None)), None);
}

#[test]
fn claim_persists_only_when_bound_to_served_principal() {
    let claim = |principal: &str| distill_config::signed_policy::ManagedIdentityClaim {
        typ: distill_config::signed_policy::MANAGED_IDENTITY_TYP.into(),
        principal: principal.into(),
        fail_closed: true,
        expires_at: 4_000_000_000,
        key_id: "v1".into(),
    };
    assert!(claim_binds_to(&claim("team-007"), Some("team-007")));
    assert!(!claim_binds_to(&claim("team-evil"), Some("team-007")));
    assert!(!claim_binds_to(&claim("team-007"), None));
}

#[test]
fn absent_claim_is_skipped() {
    assert!(verified_claim_sidecar(&ManagedConfigResponse::default(), Some("team-007")).is_none());
}

#[test]
fn auth_mode_classification() {
    use distill_telemetry::startup::AuthMode;
    let err = || std::io::Error::other("unreadable");
    assert_eq!(auth_mode(true, &Ok(true)), AuthMode::Deployment);
    assert_eq!(auth_mode(true, &Err(err())), AuthMode::Deployment);
    assert_eq!(auth_mode(false, &Ok(true)), AuthMode::Team);
    assert_eq!(auth_mode(false, &Ok(false)), AuthMode::Personal);
    assert_eq!(auth_mode(false, &Err(err())), AuthMode::Unknown);
}

#[test]
fn managed_config_gate_ignores_the_overlay_in_both_directions() {
    use crate::config::ConfigLayers;

    fn features_managed_config(v: bool) -> toml::Value {
        toml::from_str(&format!("[features]\nmanaged_config = {v}\n")).unwrap()
    }

    let mut layers = ConfigLayers {
        user: features_managed_config(true),
        env_overlay: Some(features_managed_config(false)),
        ..Default::default()
    };
    assert_eq!(managed_config_enabled_from_layers(&layers), Some(true));

    layers.user = features_managed_config(false);
    layers.env_overlay = Some(features_managed_config(true));
    assert_eq!(managed_config_enabled_from_layers(&layers), Some(false));

    layers.user = toml::Value::Table(Default::default());
    layers.env_overlay = Some(features_managed_config(false));
    assert_eq!(managed_config_enabled_from_layers(&layers), None);
}
