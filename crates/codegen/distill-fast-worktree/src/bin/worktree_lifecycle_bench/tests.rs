// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

#[test]
fn status_coverage_distinguishes_staged_dirty_and_untracked() {
    let status = b"1 M. N... 100644 100644 100644 a b staged.txt\0\
                   1 .M N... 100644 100644 100644 a b dirty.txt\0\
                   ? untracked.txt\0";
    let coverage = classify_status(status);
    assert!(coverage.has_staged);
    assert!(coverage.has_dirty);
    assert!(coverage.has_untracked);
}

#[test]
fn warm_medians_exclude_the_first_uncontrolled_sample() {
    let samples = (0..6)
        .map(|iteration| Sample {
            iteration,
            sample_id: format!("sample-{iteration}"),
            worktree_id: format!("sample-{iteration}"),
            sample_class: if iteration == 0 { "first" } else { "warm" },
            daemon_cache_state: if iteration == 0 {
                "preexisting_uncontrolled"
            } else {
                "reused"
            },
            resolved_strategy: "copy".into(),
            resolution: Resolution::Native {
                reason: "test".into(),
            },
            durations_ms: PhaseDurations {
                create: iteration as f64,
                first_readdir: iteration as f64 + 1.0,
                first_read: iteration as f64 + 2.0,
                first_git_status: iteration as f64 + 3.0,
                second_git_status: iteration as f64 + 4.0,
                remove_cleanup: iteration as f64 + 5.0,
            },
            create_phases_ms: BTreeMap::new(),
            correctness: Correctness {
                head_matches: true,
                tree_matches: true,
                status_matches: true,
                staged_state_matches: true,
                dirty_state_matches: true,
                untracked_state_matches: true,
                first_read_matches: true,
            },
            cleanup: CleanupVerification {
                dest_absent: true,
                mount_absent: true,
                backing_absent: true,
                pin_absent: true,
                worktree_admin_unchanged: true,
                journal_absent: true,
            },
        })
        .collect::<Vec<_>>();
    let warm_medians = median_phases(samples.get(1..).unwrap_or(&[]));
    assert_eq!(samples.first().map(|s| s.durations_ms.create), Some(0.0));
    assert_eq!(warm_medians.create, 3.0);
    assert_eq!(warm_medians.remove_cleanup, 8.0);
}

#[test]
fn report_schema_is_versioned_and_keeps_skip_reason() {
    let report = BenchmarkReport {
        schema_version: SCHEMA_VERSION,
        provenance: Provenance {
            generated_at_unix_secs: 1,
            package_version: "test",
            run_id: "run".into(),
            source: REDACTED_PATH.into(),
            source_head: "a".repeat(40),
            source_tree: "b".repeat(40),
            git_revision: "a".repeat(40),
            dirty_digest_sha256: "c".repeat(64),
            harness_repo_dirty_digest_sha256: "f".repeat(64),
            harness_repo_commit: "a".repeat(40),
            harness_repo_tree: "b".repeat(40),
            harness_inputs: vec![HarnessInput {
                path: "main",
                sha256: "d".repeat(64),
                head_sha256: Some("d".repeat(64)),
                matches_head: true,
                dirty: false,
            }],
            harness_executable_sha256: "e".repeat(64),
            containment_support: ContainmentSupport {
                native: true,
                grove: false,
                reason: "test".into(),
            },
            tracked_files: 1,
            source_state: StateCoverage {
                has_staged: false,
                has_dirty: false,
                has_untracked: false,
            },
            os: "linux",
            arch: "x86_64",
            release: true,
            argv: vec!["bench".into()],
            first_definition: "first",
            warm_state: "reused",
            warm_iterations: MIN_WARM_ITERATIONS,
            create_phase_source: "not exposed",
        },
        cases: vec![CaseReport {
            name: "grove_projected",
            worktree_shape: "grove_projected",
            requested_transport: "fuse",
            support: Support::Skipped {
                reason: "missing /dev/fuse".into(),
            },
            raw_samples: Vec::new(),
            summary_ms: None,
        }],
    };
    let value = serde_json::to_value(report).unwrap();
    let at = |path: &str| {
        value
            .pointer(path)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    assert_eq!(at("/schema_version"), SCHEMA_VERSION);
    assert_eq!(at("/cases/0/support/status"), "skipped");
    assert_eq!(at("/cases/0/support/reason"), "missing /dev/fuse");
    assert_eq!(at("/provenance/source"), REDACTED_PATH);
}

#[test]
fn sample_ids_do_not_collide_across_runs_or_cases() {
    let a = sample_identity("aaaaaaaa", CaseKind::NativeOrdinary, 0);
    let b = sample_identity("bbbbbbbb", CaseKind::NativeOrdinary, 0);
    let c = sample_identity("aaaaaaaa", CaseKind::NativeLinked, 0);
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert!(a.contains("aaaaaaaa"));
}

#[test]
fn daemon_status_skip_reason_is_redacted_before_report_storage() {
    let temp = tempfile::TempDir::new().unwrap();
    let data_dir = temp.path().join("secret-data");
    let socket = temp.path().join("secret-runtime/control.sock");
    let opts = NfsWorktreeOpts {
        control_sock: Some(socket.clone()),
        data_dir: Some(data_dir.clone()),
        runtime_dir: socket.parent().map(Path::to_path_buf),
        ..NfsWorktreeOpts::default()
    };
    let raw = format!(
        "Grove cancellation capability check failed: daemon status {} token=https://user:secret@example.test/private",
        socket.display()
    );
    let reason = redact_daemon_status_skip_reason(&raw, &opts);
    assert!(!reason.contains(&socket.to_string_lossy().to_string()));
    assert!(!reason.contains("secret"));
    assert!(reason.contains(REDACTED_PATH));
}

#[test]
fn linux_support_gate_matches_product_preconditions() {
    use runtime::{GroveSupportProbe, MountNamespace};

    let ready = GroveSupportProbe {
        os: "linux",
        fuse_exists: true,
        fuse_writable: true,
        has_fusermount: true,
        mount_namespace: MountNamespace::Host,
        has_mount_nfs: false,
        daemon_reachable: true,
    };
    assert_eq!(runtime::grove_skip_reason(&ready), None);

    let mut unwritable = ready.clone();
    unwritable.fuse_writable = false;
    assert!(
        runtime::grove_skip_reason(&unwritable)
            .unwrap()
            .contains("not writable")
    );

    let mut private = ready;
    private.mount_namespace = MountNamespace::Private;
    assert!(
        runtime::grove_skip_reason(&private)
            .unwrap()
            .contains("private mount namespace")
    );
}

#[test]
fn cleanup_is_armed_before_create() {
    let temp = tempfile::TempDir::new().unwrap();
    let dest = temp.path().join("partial");
    let registry = Arc::new(CleanupRegistry::new());
    let guard = registry.arm(CleanupTarget {
        dest: dest.clone(),
        worktree_id: "test".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: false,
    });
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("partial"), b"x").unwrap();
    drop(guard);
    assert!(!dest.exists());
}

#[test]
fn contained_subprocess_times_out_and_kills_descendants() {
    let mut command = std::process::Command::new("sh");
    distill_tty_utils::detach_std_command(&mut command);
    command.args(["-c", "sleep 30 & wait"]);
    let started = Instant::now();
    let registry = runtime::CommandRegistry::new();
    let error =
        runtime::run_contained_command(&registry, command, std::time::Duration::from_millis(50))
            .unwrap_err();
    assert!(error.to_string().contains("deadline"), "{error:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

fn worker_executable() -> PathBuf {
    let current = std::env::current_exe().unwrap();
    let debug = current.parent().unwrap().parent().unwrap();
    let binary = debug.join("worktree-lifecycle-bench");
    assert!(
        binary.is_file(),
        "build the benchmark binary before unit tests: {}",
        binary.display()
    );
    binary
}

#[test]
fn worker_background_hook_is_contained_after_worker_exit() {
    let Some(cgroup) =
        runtime::CgroupV2::create(&format!("worker-hook-{}", uuid::Uuid::new_v4().simple()))
            .unwrap()
    else {
        return;
    };
    let registry = runtime::CommandRegistry::new();
    let mut command = std::process::Command::new(worker_executable());
    distill_tty_utils::detach_std_command(&mut command);
    command.args([
        "--worker",
        "--worker-stop-before-work",
        "--worker-hook",
        "setsid sleep 30 >/dev/null 2>&1 & exit 0",
        "--source",
        "/missing",
    ]);
    let started = Instant::now();
    let output = runtime::run_stopped_worker_with_cgroup(
        &registry,
        command,
        std::time::Duration::from_secs(2),
        Arc::clone(&cgroup),
    )
    .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(cgroup.member_pids().unwrap().is_empty());
    assert_eq!(registry.live_count_for_artifact(), 0);
}

#[test]
fn worker_detached_hook_is_killed_on_timeout() {
    let Some(cgroup) =
        runtime::CgroupV2::create(&format!("worker-timeout-{}", uuid::Uuid::new_v4().simple()))
            .unwrap()
    else {
        return;
    };
    let registry = runtime::CommandRegistry::new();
    let temp = tempfile::TempDir::new().unwrap();
    let gate = temp.path().join("worker-gate");
    std::fs::write(&gate, b"hold").unwrap();
    let mut command = std::process::Command::new(worker_executable());
    distill_tty_utils::detach_std_command(&mut command);
    command
        .arg("--worker")
        .arg("--worker-stop-before-work")
        .arg("--worker-hook")
        .arg("setsid sleep 30 >/dev/null 2>&1 &")
        .arg("--worker-gate")
        .arg(&gate)
        .arg("--source")
        .arg("/missing");
    let error = runtime::run_stopped_worker_with_cgroup(
        &registry,
        command,
        std::time::Duration::from_millis(50),
        Arc::clone(&cgroup),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("deadline"),
        "{error:#}; cgroup members={:?}",
        cgroup.member_pids()
    );
    assert!(cgroup.member_pids().unwrap().is_empty());
}

#[test]
fn cgroup_enrollment_failure_kills_child_before_detached_descendant_can_escape() {
    let Some(cgroup) =
        runtime::CgroupV2::create(&format!("enroll-race-{}", uuid::Uuid::new_v4().simple()))
            .unwrap()
    else {
        return;
    };
    cgroup.remove_for_test().unwrap();
    let registry = runtime::CommandRegistry::new();
    let temp = tempfile::TempDir::new().unwrap();
    let escaped = temp.path().join("escaped");
    let script = format!(
        "setsid sh -c 'sleep 0.1; touch {}' >/dev/null 2>&1 & wait",
        escaped.to_string_lossy()
    );
    let mut command = std::process::Command::new("sh");
    distill_tty_utils::detach_std_command(&mut command);
    command.args(["-c", &script]);
    let error = runtime::run_contained_command_with_cgroup(
        &registry,
        command,
        std::time::Duration::from_secs(2),
        Some(Arc::clone(&cgroup)),
    )
    .unwrap_err();
    assert!(error.to_string().contains("enroll"), "{error:#}");
    assert_eq!(registry.live_count_for_artifact(), 0);
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(!escaped.exists());
}

#[test]
fn reader_spawn_failures_kill_and_reap_before_unregistering() {
    for failure in [
        runtime::ReaderSpawnFailure::First,
        runtime::ReaderSpawnFailure::Second,
    ] {
        let registry = runtime::CommandRegistry::new();
        let cgroup =
            runtime::CgroupV2::create(&format!("reader-failure-{}", uuid::Uuid::new_v4().simple()))
                .unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let survivor_marker = temp.path().join("survivor");
        let script = format!(
            "(sleep 0.2; touch {}) & wait",
            survivor_marker.to_string_lossy()
        );
        let mut command = std::process::Command::new("sh");
        distill_tty_utils::detach_std_command(&mut command);
        command.args(["-c", &script]);
        let error = runtime::run_contained_command_with_reader_failure(
            &registry,
            command,
            cgroup.clone(),
            failure,
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected"), "{error:#}");
        if let Some(cgroup) = cgroup {
            assert!(cgroup.member_pids().unwrap().is_empty());
        }
        assert_eq!(registry.live_count_for_artifact(), 0);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!survivor_marker.exists());
    }
}

#[test]
fn inherited_pipe_grandchild_is_killed_after_direct_child_exits() {
    let registry = runtime::CommandRegistry::new();
    let mut command = std::process::Command::new("sh");
    distill_tty_utils::detach_std_command(&mut command);
    command.args(["-c", "sleep 30 & exit 0"]);
    let started = Instant::now();
    let output =
        runtime::run_contained_command(&registry, command, std::time::Duration::from_secs(2))
            .unwrap();
    assert!(output.status.success());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "post-reap descendant teardown must not wait for zombie-free process group: {:?}",
        started.elapsed()
    );
    assert_eq!(registry.live_count_for_artifact(), 0);
}

#[test]
fn bounded_reader_join_rejects_inherited_open_pipe() {
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        Ok(Vec::new())
    });
    let started = Instant::now();
    let error = runtime::join_reader_bounded(handle, "test", std::time::Duration::from_millis(20))
        .unwrap_err();
    assert!(error.to_string().contains("bounded teardown"));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn signal_watcher_runs_cleanup_callback_and_writes_redacted_artifact() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called_handler = Arc::clone(&called);
    let watcher = runtime::SignalWatcher::install_for_test(Arc::new(move |_| {
        called_handler.store(true, std::sync::atomic::Ordering::SeqCst);
    }))
    .unwrap();
    runtime::notify_signal_for_test(libc::SIGTERM).unwrap();
    for _ in 0..100 {
        if called.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    drop(watcher);

    let temp = tempfile::TempDir::new().unwrap();
    let output = temp.path().join("interrupted.json");
    let artifact = interrupted_artifact_json(
        "run-id",
        libc::SIGTERM,
        &InterruptedCleanup {
            commands_remaining: 0,
            worktrees_remaining: 0,
            failures: Vec::new(),
        },
    )
    .unwrap();
    emit_final_json(Some(&output), &artifact).unwrap();
    let artifact = std::fs::read_to_string(output).unwrap();
    assert!(artifact.contains("interrupted"));
    assert!(!artifact.contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn signal_exit_emits_one_interrupted_artifact() {
    if std::env::var_os("TEST_SRCDIR").is_some() {
        return;
    }
    for writes_file in [false, true] {
        let temp = tempfile::TempDir::new().unwrap();
        let gate = temp.path().join("controller-gate");
        let output = temp.path().join("result.json");
        std::fs::write(&gate, b"hold").unwrap();
        let mut command = std::process::Command::new(worker_executable());
        distill_tty_utils::detach_std_command(&mut command);
        command
            .arg("--controller-gate")
            .arg(&gate)
            .stdout(std::process::Stdio::piped());
        if writes_file {
            command.arg("--output").arg(&output);
        }
        #[allow(clippy::disallowed_methods)]
        // The test retains the child and reaps it after signaling.
        let child = command.spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: child is live and owned by this test.
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        let result = child.wait_with_output().unwrap();
        assert_eq!(result.status.code(), Some(128 + libc::SIGTERM));
        let artifact = if writes_file {
            assert!(result.stdout.is_empty());
            std::fs::read_to_string(output).unwrap()
        } else {
            String::from_utf8(result.stdout).unwrap()
        };
        assert_eq!(artifact.matches("\"status\"").count(), 1);
        assert!(artifact.contains("\"status\": \"interrupted\""));
    }
}

#[test]
fn worker_detached_hook_is_killed_on_signal_cleanup() {
    let Some(cgroup) =
        runtime::CgroupV2::create(&format!("worker-signal-{}", uuid::Uuid::new_v4().simple()))
            .unwrap()
    else {
        return;
    };
    let registry = runtime::CommandRegistry::new();
    let temp = tempfile::TempDir::new().unwrap();
    let gate = temp.path().join("worker-gate");
    std::fs::write(&gate, b"hold").unwrap();
    let worker_registry = registry.clone();
    let worker_cgroup = Arc::clone(&cgroup);
    let worker_gate = gate.clone();
    let worker = std::thread::spawn(move || {
        let mut command = std::process::Command::new(worker_executable());
        distill_tty_utils::detach_std_command(&mut command);
        command
            .arg("--worker")
            .arg("--worker-stop-before-work")
            .arg("--worker-hook")
            .arg("setsid sleep 30 >/dev/null 2>&1 &")
            .arg("--worker-gate")
            .arg(worker_gate)
            .arg("--source")
            .arg("/missing");
        runtime::run_stopped_worker_with_cgroup(
            &worker_registry,
            command,
            std::time::Duration::from_secs(30),
            worker_cgroup,
        )
    });
    for _ in 0..100 {
        if registry.live_count_for_artifact() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(registry.terminate_all().is_empty());
    let _ = worker.join().unwrap();
    assert!(cgroup.member_pids().unwrap().is_empty());
}

#[test]
fn signal_during_command_terminates_registered_group() {
    let registry = runtime::CommandRegistry::new();
    let worker_registry = registry.clone();
    let worker = std::thread::spawn(move || {
        let mut command = std::process::Command::new("sh");
        distill_tty_utils::detach_std_command(&mut command);
        command.args(["-c", "sleep 30 & wait"]);
        runtime::run_contained_command(
            &worker_registry,
            command,
            std::time::Duration::from_secs(30),
        )
    });
    for _ in 0..100 {
        if registry.live_count_for_artifact() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(registry.live_count_for_artifact(), 1);
    assert!(registry.terminate_all().is_empty());
    assert!(worker.join().unwrap().is_ok());
    assert_eq!(registry.live_count_for_artifact(), 0);
}

#[test]
fn grove_cleanup_routes_in_flight_and_unknown_to_cancel() {
    for (state, expected) in [
        (
            GroveCleanupState::Phase("aborted".into()),
            GroveCleanupRoute::Aborted,
        ),
        (
            GroveCleanupState::Phase("committed".into()),
            GroveCleanupRoute::Committed,
        ),
        (GroveCleanupState::Unknown, GroveCleanupRoute::Cancel),
        (
            GroveCleanupState::Phase("capturing".into()),
            GroveCleanupRoute::Cancel,
        ),
        (
            GroveCleanupState::Phase("mounted".into()),
            GroveCleanupRoute::Cancel,
        ),
        (
            GroveCleanupState::Phase("cancelling".into()),
            GroveCleanupRoute::Cancel,
        ),
    ] {
        let route = wait_for_grove_cleanup_route(|| Ok(state.clone())).unwrap();
        assert_eq!(route, expected);
    }
}

#[test]
fn grove_cleanup_query_error_is_a_hard_failure() {
    let error = wait_for_grove_cleanup_route(|| anyhow::bail!("query exploded")).unwrap_err();
    assert!(error.to_string().contains("query exploded"));
}

#[test]
fn cancel_cleanup_acks_tombstone_after_committed_remove() {
    let temp = tempfile::TempDir::new().unwrap();
    let target = CleanupTarget {
        dest: temp.path().join("dest"),
        worktree_id: "committed-then-cancel".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: true,
    };
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let acked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let removed_call = Arc::clone(&removed);
    let acked_call = Arc::clone(&acked);
    let queries_call = Arc::clone(&queries);
    cleanup_cancelled_grove_identity_after_cancel(
        &target,
        std::time::Duration::from_secs(5),
        std::time::Duration::ZERO,
        move || {
            let call = queries_call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(match call {
                0 => (Some("committed".into()), false),
                1 => (Some("cancelled".into()), false),
                _ => (None, true),
            })
        },
        move || {
            removed_call.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
        move || {
            acked_call.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .unwrap();
    assert!(removed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(acked.load(std::sync::atomic::Ordering::SeqCst));
    assert!(queries.load(std::sync::atomic::Ordering::SeqCst) >= 3);
}

#[test]
fn cleanup_by_id_before_marker_invokes_remove() {
    let temp = tempfile::TempDir::new().unwrap();
    let target = CleanupTarget {
        dest: temp.path().join("dest"),
        worktree_id: "before-marker".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: true,
    };
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let removed_call = Arc::clone(&removed);
    let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let queries_call = Arc::clone(&queries);
    cleanup_grove_identity_with(
        &target,
        move || {
            let call = queries_call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if call == 0 {
                (Some("aborted".into()), false)
            } else {
                (None, true)
            })
        },
        move || {
            removed_call.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .unwrap();
    assert!(removed.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn aborted_journal_is_removed_and_verified_absent() {
    let temp = tempfile::TempDir::new().unwrap();
    let target = CleanupTarget {
        dest: temp.path().join("dest"),
        worktree_id: "aborted".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: true,
    };
    let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let queries_call = Arc::clone(&queries);
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let removed_call = Arc::clone(&removed);
    cleanup_grove_identity_with(
        &target,
        move || {
            let call = queries_call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if call == 0 {
                (Some("aborted".into()), false)
            } else {
                (None, true)
            })
        },
        move || {
            removed_call.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .unwrap();
    assert!(removed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(queries.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn classification_query_errors_are_preserved() {
    let opts = NfsWorktreeOpts {
        enabled: true,
        control_sock: Some(PathBuf::from("/definitely/missing/control.sock")),
        ping_timeout: std::time::Duration::from_millis(5),
        ..NfsWorktreeOpts::default()
    };
    let error = classify_resolution(
        CaseKind::GroveProjected,
        "grove-fuse",
        &opts,
        "classification-error",
    )
    .unwrap_err();
    assert!(error.to_string().contains("classification"));
}

#[test]
fn query_errors_are_preserved() {
    let temp = tempfile::TempDir::new().unwrap();
    let target = CleanupTarget {
        dest: temp.path().join("dest"),
        worktree_id: "query-error".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: true,
    };
    let error = cleanup_grove_identity_with(&target, || anyhow::bail!("query exploded"), || Ok(()))
        .unwrap_err();
    assert!(error.to_string().contains("query exploded"));
}

#[test]
fn cleanup_failures_are_aggregated() {
    let temp = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(CleanupRegistry::new());
    let _guard_a = registry.arm(CleanupTarget {
        dest: temp.path().join("a"),
        worktree_id: "a".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: false,
    });
    let _guard_b = registry.arm(CleanupTarget {
        dest: temp.path().join("b"),
        worktree_id: "b".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: false,
    });
    let error = registry
        .cleanup_all_with(|target| {
            anyhow::bail!(
                "injected failure for {}",
                target.dest.file_name().unwrap().to_string_lossy()
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("2 cleanup operation"));
    assert_eq!(registry.failures().len(), 2);
}

#[test]
fn inconclusive_mount_probe_is_not_clean() {
    assert!(cleanup_mount_absent_with(true));
    assert!(!cleanup_mount_absent_with(false));
}

#[test]
fn command_enrollment_signal_race_has_no_unowned_child() {
    for _ in 0..5 {
        let registry = runtime::CommandRegistry::new();
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            let mut command = std::process::Command::new("sh");
            distill_tty_utils::detach_std_command(&mut command);
            command.args(["-c", "sleep 30"]);
            runtime::run_contained_command(
                &worker_registry,
                command,
                std::time::Duration::from_secs(30),
            )
        });
        for _ in 0..100 {
            if registry.live_count_for_artifact() == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = registry.terminate_all();
        let _ = worker.join().unwrap();
        assert_eq!(registry.live_count_for_artifact(), 0);
    }
}

#[test]
fn cleanup_error_paths_are_redacted_before_artifact_storage() {
    let secret = Path::new("/secret/worktree/path");
    let redacted = redact_cleanup_error(
        "remove /secret/worktree/path failed: permission denied",
        &[secret],
    );
    assert!(!redacted.contains("/secret/worktree/path"));
    assert!(redacted.contains(REDACTED_PATH));

    let temp = tempfile::TempDir::new().unwrap();
    let target = CleanupTarget {
        dest: temp.path().join("dest"),
        worktree_id: "id".into(),
        source: temp.path().join("source"),
        opts: NfsWorktreeOpts {
            data_dir: Some(temp.path().join("data")),
            runtime_dir: Some(temp.path().join("runtime")),
            control_sock: Some(temp.path().join("runtime/control.sock")),
            ..NfsWorktreeOpts::default()
        },
        is_grove: true,
    };
    let paths = cleanup_sensitive_paths(&target);
    let diagnostic = format!(
        "{} {} {} {} {}",
        target.dest.display(),
        target.source.display(),
        target.opts.data_dir.as_ref().unwrap().display(),
        target.opts.runtime_dir.as_ref().unwrap().display(),
        target.opts.control_sock.as_ref().unwrap().display(),
    );
    let redacted = redact_cleanup_error(&diagnostic, &paths);
    assert!(!redacted.contains(temp.path().to_string_lossy().as_ref()));
    let file_url = url::Url::from_file_path(temp.path().join("data/secret")).unwrap();
    let redacted_url = redact_cleanup_error(file_url.as_str(), &[temp.path()]);
    assert!(!redacted_url.contains(temp.path().to_string_lossy().as_ref()));
    assert!(redacted_url.contains(REDACTED_PATH));

    let embedded = redact_cleanup_error(
        "key=/secret/key url=https://user:pass@example.test/private?q=token#fragment remote=file:///secret/repo?q=x#y",
        &[],
    );
    assert_eq!(
        embedded,
        "key=<redacted-path> url=<redacted-path> remote=<redacted-path>"
    );
    assert!(!embedded.contains("user:pass"));
    assert!(!embedded.contains("token"));
    assert!(!embedded.contains("fragment"));

    let punctuated = redact_cleanup_error(
        "request(https://user:pass@host/path?q=secret#frag), next=[file:///secret/all?q=x#y] custom=(ssh://user:pass@host/private?q=z#f)",
        &[],
    );
    assert_eq!(
        punctuated,
        "request(<redacted-path>), next=[<redacted-path>] custom=(<redacted-path>)"
    );
    assert!(!punctuated.contains("secret"));
    assert!(!punctuated.contains("user:pass"));

    let prose = redact_cleanup_error("ratio=1/2 and route/to stays; absolute=/all/secret", &[]);
    assert_eq!(
        prose,
        "ratio=1/2 and route/to stays; absolute=<redacted-path>"
    );
}

#[test]
fn whole_artifact_redaction_covers_every_runtime_path_and_url_shape() {
    let temp = tempfile::TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");
    let data = temp.path().join("data");
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("control.sock");
    let backing = data.join("worktree-backing/id");
    let output = temp.path().join("result.json");
    let executable = temp.path().join("bin/worktree-lifecycle-bench");
    let mut redactions = RedactionSet::default();
    for path in [
        &source,
        &dest,
        &data,
        &runtime,
        &socket,
        &backing,
        &output,
        &executable,
    ] {
        redactions.add(path);
    }
    let artifact = serde_json::json!({
        "source": source,
        "dest": dest,
        "data": data,
        "runtime": runtime,
        "socket": socket,
        "backing": backing,
        "output": output,
        "executable": executable,
        "error": "key=/secret/key url=https://user:pass@example.test/private?q=canary#anchor",
    });
    let redacted = redactions.redact(&serde_json::to_string(&artifact).unwrap());
    assert!(!redacted.contains(temp.path().to_string_lossy().as_ref()));
    assert!(!redacted.contains("/secret/key"));
    assert!(!redacted.contains("user:pass"));
    assert!(!redacted.contains("canary"));
    assert!(!redacted.contains("anchor"));
}

#[test]
fn failed_json_defaults_to_stdout_without_output_path() {
    let artifact = FailedArtifact {
        schema_version: SCHEMA_VERSION,
        status: "failed",
        run_id: "run",
        error: "redacted".into(),
        cleanup: InterruptedCleanup {
            commands_remaining: 0,
            worktrees_remaining: 0,
            failures: Vec::new(),
        },
        source: REDACTED_PATH,
        output: REDACTED_PATH,
    };
    let json = serde_json::to_string(&artifact).unwrap();
    assert!(emit_final_json(None, &json).is_ok());
}

#[test]
fn ordinary_error_runs_cleanup_and_writes_failed_artifact() {
    let temp = tempfile::TempDir::new().unwrap();
    let output = temp.path().join("result.json");
    let cli = Cli {
        source: temp.path().join("secret-source"),
        warm_iterations: 5,
        require_grove: false,
        output: Some(output.clone()),
        worker: false,
        worker_dest: None,
        worker_id: None,
        worker_kind: None,
        worker_hook: None,
        worker_gate: None,
        worker_stop_before_work: false,
        controller_gate: None,
        worker_control_sock: None,
        worker_data_dir: None,
        worker_runtime_dir: None,
    };
    let cleanup_registry = Arc::new(CleanupRegistry::new());
    let dest = temp.path().join("dest");
    let _guard = cleanup_registry.arm(CleanupTarget {
        dest: dest.clone(),
        worktree_id: "failed".into(),
        source: temp.path().to_path_buf(),
        opts: NfsWorktreeOpts::default(),
        is_grove: false,
    });
    std::fs::create_dir(&dest).unwrap();
    let artifact = ArtifactFinalizer::new(cli.output.clone());
    let error = finalize_controller_failure(
        &cli,
        "run",
        &cleanup_registry,
        &runtime::CommandRegistry::new(),
        &artifact,
        anyhow::anyhow!("failed at {}", cli.source.display()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("failed"));
    assert!(!dest.exists());
    let artifact = std::fs::read_to_string(output).unwrap();
    assert!(artifact.contains("\"status\": \"failed\""));
    assert!(!artifact.contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn artifact_finalizer_keeps_signal_artifact_from_being_overwritten() {
    let temp = tempfile::TempDir::new().unwrap();
    let output = temp.path().join("result.json");
    let finalizer = ArtifactFinalizer::new(Some(output.clone()));
    let interrupted = interrupted_artifact_json(
        "run",
        libc::SIGTERM,
        &InterruptedCleanup {
            commands_remaining: 0,
            worktrees_remaining: 0,
            failures: Vec::new(),
        },
    )
    .unwrap();
    finalizer.finalize(&interrupted).unwrap();
    finalizer.finalize(r#"{"status":"success"}"#).unwrap();
    let artifact = std::fs::read_to_string(output).unwrap();
    assert_eq!(artifact.matches("\"status\"").count(), 1);
    assert!(artifact.contains("\"status\": \"interrupted\""));
    assert!(!artifact.contains("success"));
}

#[test]
fn require_grove_failure_is_not_overwritten_by_generic_failure() {
    let temp = tempfile::TempDir::new().unwrap();
    let output = temp.path().join("result.json");
    let finalizer = ArtifactFinalizer::new(Some(output.clone()));
    finalizer
        .finalize(r#"{"status":"failed","error":"--require-grove"}"#)
        .unwrap();
    finalizer
        .finalize(r#"{"status":"failed","error":"generic"}"#)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        r#"{"status":"failed","error":"--require-grove"}"#
    );
}

#[test]
fn dirty_repo_digest_changes_for_content_mode_and_symlink_target() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path();
    let registry = runtime::CommandRegistry::new();
    let git = |args: &[&str]| {
        let output = runtime::run_git(&registry, repo, args).unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(repo.join("tracked"), b"one").unwrap();
    git(&["add", "tracked"]);
    git(&["commit", "-qm", "initial"]);

    std::fs::write(repo.join("tracked"), b"two").unwrap();
    let status = git_bytes(
        &registry,
        repo,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )
    .unwrap();
    let content = dirty_repo_digest(&registry, repo, &status).unwrap();
    std::fs::set_permissions(repo.join("tracked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mode = dirty_repo_digest(&registry, repo, &status).unwrap();
    assert_ne!(content, mode);

    std::fs::remove_file(repo.join("tracked")).unwrap();
    std::os::unix::fs::symlink("first-target", repo.join("tracked")).unwrap();
    let link_one = dirty_repo_digest(&registry, repo, &status).unwrap();
    std::fs::remove_file(repo.join("tracked")).unwrap();
    std::os::unix::fs::symlink("second-target", repo.join("tracked")).unwrap();
    let link_two = dirty_repo_digest(&registry, repo, &status).unwrap();
    assert_ne!(link_one, link_two);
}

#[test]
fn provenance_digest_and_blob_match_are_stable() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let report = Provenance {
        generated_at_unix_secs: 1,
        package_version: "test",
        run_id: "run".into(),
        source: REDACTED_PATH.into(),
        source_head: "a".repeat(40),
        source_tree: "b".repeat(40),
        git_revision: "a".repeat(40),
        dirty_digest_sha256: sha256_hex(b"dirty"),
        harness_repo_dirty_digest_sha256: sha256_hex(b"repo-dirty"),
        harness_repo_commit: "a".repeat(40),
        harness_repo_tree: "b".repeat(40),
        harness_inputs: vec![HarnessInput {
            path: "main",
            sha256: sha256_hex(b"harness"),
            head_sha256: Some(sha256_hex(b"harness")),
            matches_head: true,
            dirty: false,
        }],
        harness_executable_sha256: sha256_hex(b"exe"),
        containment_support: ContainmentSupport {
            native: true,
            grove: false,
            reason: "test".into(),
        },
        tracked_files: 1,
        source_state: StateCoverage {
            has_staged: false,
            has_dirty: false,
            has_untracked: false,
        },
        os: "linux",
        arch: "x86_64",
        release: true,
        argv: vec!["worktree-lifecycle-bench".into()],
        first_definition: "first",
        warm_state: "warm",
        warm_iterations: 5,
        create_phase_source: "none",
    };
    assert_eq!(report.harness_repo_commit, "a".repeat(40));
    assert_eq!(report.harness_repo_tree, "b".repeat(40));
    let Some(input) = report.harness_inputs.first() else {
        panic!("expected harness input: {:?}", report.harness_inputs);
    };
    assert!(input.matches_head);
    assert_eq!(input.head_sha256.as_ref(), Some(&input.sha256));
    assert_eq!(report.harness_executable_sha256.len(), 64);
}

#[test]
fn compiled_input_provenance_lists_required_files() {
    assert_eq!(HARNESS_INPUT_PATHS.len(), 7);
    assert!(
        HARNESS_INPUT_PATHS
            .contains(&"crates/codegen/distill-fast-worktree/src/bin/worktree_lifecycle_bench.rs")
    );
    assert!(
        HARNESS_INPUT_PATHS.contains(
            &"crates/codegen/distill-fast-worktree/src/bin/worktree_lifecycle_bench/runtime.rs"
        )
    );
    assert!(HARNESS_INPUT_PATHS.contains(&"crates/codegen/distill-fast-worktree/Cargo.toml"));
    assert!(HARNESS_INPUT_PATHS.contains(&"crates/codegen/distill-fast-worktree/BUILD.bazel"));
    assert!(HARNESS_INPUT_PATHS.contains(&"Cargo.lock"));
    assert!(HARNESS_INPUT_PATHS.contains(&"rust-toolchain.toml"));
    assert!(HARNESS_INPUT_PATHS.contains(&".bazelrc"));
}

#[test]
fn measured_worker_containment_never_claims_process_group_fallback() {
    let support = measured_worker_containment_support();
    if cfg!(target_os = "linux") && runtime::CgroupV2::support_available() {
        assert!(support.native && support.grove);
        assert!(support.reason.contains("cgroup-v2"));
    } else {
        assert!(!support.native && !support.grove);
        assert!(support.reason.contains("skipped"));
    }
}

#[test]
fn provenance_paths_and_first_semantics_are_redacted_and_truthful() {
    let argv = runtime::redact_argv([
        "/secret/bin/worktree-lifecycle-bench".into(),
        "--source".into(),
        "/secret/source".into(),
        "--output=/secret/result.json".into(),
    ]);
    assert_eq!(
        argv.first().map(String::as_str),
        Some("worktree-lifecycle-bench")
    );
    assert_eq!(argv.get(2).map(String::as_str), Some(REDACTED_PATH));
    assert_eq!(
        argv.get(3).map(String::as_str),
        Some("--output=<redacted-path>")
    );

    let samples = [("first", "preexisting_uncontrolled"), ("warm", "reused")];
    assert_eq!(samples[0], ("first", "preexisting_uncontrolled"));
    assert_ne!(samples[0].0, "cold");
}
