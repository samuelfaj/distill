use super::*;

fn workflow(name: &str, body: &str) -> String {
    format!("let meta = #{{ name: \"{name}\", description: \"test\" }};\n{body}\n")
}

fn path_keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn permits() -> std::sync::Arc<tokio::sync::Semaphore> {
    std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CHECKS))
}

#[test]
fn path_param_names_come_from_kind_template_map() {
    let mut write = std::collections::HashMap::new();
    write.insert("file_path".to_owned(), "src".to_owned());
    write.insert("content".to_owned(), "body".to_owned());
    let mut params = std::collections::HashMap::new();
    params.insert(ToolKind::Write, write);

    assert_eq!(
        path_param_names_for_kind(ToolKind::Write, &params),
        vec!["src".to_owned()]
    );
    assert_eq!(
        path_param_names_for_kind(ToolKind::Edit, &params),
        path_keys(&["file_path", "path", "target_file"])
    );
}

#[test]
fn last_smoke_check_keeps_final_edit_per_path() {
    let indices = last_smoke_check_indices([
        (0, Some(".grok/workflows/a.rhai".to_owned())),
        (1, Some(".grok/workflows/b.rhai".to_owned())),
        (2, Some(".grok/workflows/a.rhai".to_owned())),
        (3, None),
    ]);
    assert_eq!(indices, std::collections::HashSet::from([1, 2]));
}

#[test]
fn path_helper_matches_only_grok_workflow_rhai_files() {
    for path in [
        ".grok/workflows/check.rhai",
        "project/.grok/workflows/check.rhai",
        r"C:\project\.grok\workflows\check.rhai",
    ] {
        let normalized = path.replace('\\', "/");
        assert!(
            is_project_workflow_rhai_path(Path::new(&normalized)),
            "{path}"
        );
    }

    for path in [
        ".grok/check.rhai",
        ".grok/workflows/check.txt",
        ".grok/not-workflows/check.rhai",
        "workflows/check.rhai",
    ] {
        assert!(!is_project_workflow_rhai_path(Path::new(path)), "{path}");
    }
}

#[tokio::test]
async fn valid_written_workflow_needs_no_warning() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(".grok/workflows/valid.rhai");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, workflow("valid", "complete(\"ok\");")).unwrap();
    crate::agent::folder_trust::record_for_test(directory.path(), true);

    let snapshot = snapshot_authored_workflow(
        Some(ToolKind::Write),
        &serde_json::json!({ "file_path": path }),
        &path_keys(&["file_path"]),
        directory.path(),
        None,
        directory.path(),
    )
    .await
    .expect("workflow path")
    .expect("workflow resolution");

    assert_eq!(check_snapshot(snapshot, &permits()).await, None);
}

#[tokio::test]
async fn invalid_authored_workflow_returns_path_specific_warning() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(".grok/workflows/broken.rhai");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, workflow("broken", "unknown_host_function();")).unwrap();
    crate::agent::folder_trust::record_for_test(directory.path(), true);

    let snapshot = snapshot_authored_workflow(
        Some(ToolKind::Write),
        &serde_json::json!({ "file_path": ".grok/workflows/broken.rhai" }),
        &path_keys(&["file_path"]),
        directory.path(),
        None,
        directory.path(),
    )
    .await
    .expect("workflow path")
    .expect("workflow resolution");
    let failure = check_snapshot(snapshot, &permits())
        .await
        .expect("invalid workflow should fail its smoke check");
    let mut prompt = "The file was updated.".to_owned();
    append_validation_warning(
        &mut prompt,
        &failure,
        "workflow",
        "script_path",
        "validate_only",
    );

    assert_eq!(failure.path, path);
    assert!(prompt.contains("The current workflow fails smoke checks"));
    assert!(prompt.contains("workflow(script_path="));
    assert!(prompt.contains("validate_only=true"));
    assert!(!prompt.contains(&failure.detail));
    assert!(!prompt.contains("Smoke check:"));
    assert!(!prompt.contains("automatic smoke check failed"));

    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Edit),
            &serde_json::json!({ "file_path": ".grok/workflows/broken.rhai" }),
            &path_keys(&["file_path"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_some()
    );
    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Write),
            &serde_json::json!({ "path": ".grok/workflows/broken.rhai" }),
            &path_keys(&["path"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_some()
    );
    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Edit),
            &serde_json::json!({ "target_file": ".grok/workflows/broken.rhai" }),
            &path_keys(&["target_file"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_some()
    );
    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Write),
            &serde_json::json!({ "src": ".grok/workflows/broken.rhai" }),
            &path_keys(&["src"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_some()
    );
}

#[tokio::test]
async fn parse_failure_is_returned_during_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    // Unique workspace_key even when $TMPDIR lives inside a git checkout.
    git2::Repository::init(directory.path()).unwrap();
    let path = directory.path().join(".grok/workflows/parse-failure.rhai");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, workflow("parse-failure", "let value = ;")).unwrap();
    crate::agent::folder_trust::record_for_test(directory.path(), true);

    match crate::session::workflow::registry::resolve_by_path(
        &path,
        directory.path(),
        Some(directory.path()),
    ) {
        Ok(_) => panic!("parse failure should come from resolve"),
        Err(direct_err) => assert!(
            direct_err.to_string().contains("failed to parse"),
            "direct resolve error: {direct_err}"
        ),
    }

    let snapshot = snapshot_authored_workflow(
        Some(ToolKind::Write),
        &serde_json::json!({ "file_path": path }),
        &path_keys(&["file_path"]),
        directory.path(),
        None,
        directory.path(),
    )
    .await
    .expect("workflow path");
    assert!(
        snapshot.is_err(),
        "parse failure should be reported during snapshot, got Ok"
    );
}

#[tokio::test]
async fn nonterminating_workflow_times_out_promptly() {
    let snapshot = AuthoredWorkflowSnapshot {
        path: PathBuf::from(".grok/workflows/loop.rhai"),
        script: workflow("loop", "loop {}"),
    };
    let started = std::time::Instant::now();

    let failure = check_snapshot(snapshot, &permits())
        .await
        .expect("nonterminating workflow should time out");

    assert_eq!(failure.detail, "smoke check exceeded 100 ms");
    assert!(started.elapsed() >= CHECK_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn dropped_check_releases_permit_after_cancel() {
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let snapshot = AuthoredWorkflowSnapshot {
        path: PathBuf::from(".grok/workflows/loop.rhai"),
        script: workflow("loop", "loop {}"),
    };
    tokio::select! {
        _ = check_snapshot(snapshot, &permits) => {
            panic!("nonterminating check should still be running")
        }
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }

    let started = std::time::Instant::now();
    drop(permits.acquire().await.expect("permit after cancel"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn irrelevant_tool_or_path_skips_smoke_check() {
    let directory = tempfile::tempdir().unwrap();
    let missing = serde_json::json!({ "file_path": ".grok/workflows/missing.rhai" });

    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Read),
            &missing,
            &path_keys(&["file_path"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_none()
    );
    assert!(
        snapshot_authored_workflow(
            Some(ToolKind::Write),
            &serde_json::json!({ "file_path": "src/workflow.rhai" }),
            &path_keys(&["file_path"]),
            directory.path(),
            None,
            directory.path(),
        )
        .await
        .is_none()
    );
}
