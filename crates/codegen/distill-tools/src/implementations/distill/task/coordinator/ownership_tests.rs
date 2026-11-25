// Modified for Distill by Samuel Fajreldines, 2026.
use distill_tool_types::SubagentIsolationMode;

use super::super::tests::{Harness, harness, parent_backend, request};
use crate::implementations::distill::task::backend::SubagentBackend;
use crate::implementations::distill::task::file_ownership::SiblingFiles;
use crate::implementations::distill::task::types::{SubagentRequest, SubagentResult};

const LONG: std::time::Duration = std::time::Duration::from_secs(60);

fn owner(id: &str, description: &str, files: &[&str], worktree: bool) -> SubagentRequest {
    let mut child = request(id, true);
    child.description = description.to_owned();
    child.runtime_overrides.declared_files = files.iter().map(|f| (*f).to_owned()).collect();
    child.runtime_overrides.isolation = worktree.then_some(SubagentIsolationMode::Worktree);
    child
}

/// Spawn and wait until the runner has the child.
async fn start(
    harness: &mut Harness,
    child: SubagentRequest,
) -> tokio::task::JoinHandle<Result<SubagentResult, distill_tool_runtime::ToolError>> {
    let backend = parent_backend(harness);
    let handle = tokio::spawn(async move { backend.spawn(child, None).await });
    harness.requests.recv().await.expect("runner got child");
    handle
}

async fn rejected(harness: &Harness, child: SubagentRequest) -> String {
    let result = parent_backend(harness)
        .spawn(child, None)
        .await
        .expect("spawn reply");
    assert!(!result.success, "spawn must be rejected");
    result.error.expect("rejection carries an error")
}

#[tokio::test]
async fn overlapping_sibling_is_rejected_naming_owner_and_remedies() {
    let mut harness = harness(false, LONG);
    start(
        &mut harness,
        owner("a", "refactor parser", &["/r/src/a.rs"], false),
    )
    .await;

    let error = rejected(&harness, owner("b", "fix lexer", &["/r/src/a.rs"], false)).await;

    assert!(error.contains("'refactor parser'"), "{error}");
    assert!(error.contains("/r/src/a.rs"), "{error}");
    assert!(error.contains("isolation=\"worktree\""), "{error}");
    assert!(
        harness.requests.try_recv().is_err(),
        "rejected child must never run"
    );
}

#[tokio::test]
async fn directory_declaration_overlaps_file_inside_it() {
    let mut harness = harness(false, LONG);
    start(&mut harness, owner("a", "own src", &["/r/src"], false)).await;

    let error = rejected(&harness, owner("b", "edit file", &["/r/src/a.rs"], false)).await;

    assert!(error.contains("'own src'"), "{error}");
}

#[tokio::test]
async fn worktree_spawn_is_never_rejected() {
    let mut harness = harness(false, LONG);
    start(&mut harness, owner("a", "owner", &["/r/src/a.rs"], false)).await;

    start(&mut harness, owner("b", "isolated", &["/r/src/a.rs"], true)).await;
}

#[tokio::test]
async fn worktree_owner_does_not_block_shared_workspace_spawn() {
    let mut harness = harness(false, LONG);
    start(
        &mut harness,
        owner("a", "isolated owner", &["/r/src/a.rs"], true),
    )
    .await;

    start(&mut harness, owner("b", "shared", &["/r/src/a.rs"], false)).await;
}

#[tokio::test]
async fn disjoint_files_run_and_child_sees_live_sibling() {
    let mut harness = harness(false, LONG);
    start(
        &mut harness,
        owner("a", "refactor parser", &["/r/src/a.rs"], false),
    )
    .await;
    let backend = parent_backend(&harness);
    let child = owner("b", "fix lexer", &["/r/src/b.rs"], false);
    tokio::spawn(async move { backend.spawn(child, None).await });

    let seen = harness.requests.recv().await.expect("second child runs");

    assert_eq!(seen.id, "b");
    assert_eq!(
        seen.runtime_overrides.live_siblings,
        vec![SiblingFiles {
            description: "refactor parser".into(),
            files: vec!["/r/src/a.rs".into()],
        }]
    );
}

#[tokio::test]
async fn first_child_sees_no_siblings() {
    let mut harness = harness(false, LONG);
    let backend = parent_backend(&harness);
    tokio::spawn(async move {
        backend
            .spawn(owner("a", "solo", &["/r/a.rs"], false), None)
            .await
    });

    let seen = harness.requests.recv().await.expect("child runs");

    assert!(seen.runtime_overrides.live_siblings.is_empty());
}

#[tokio::test]
async fn child_without_declared_files_is_not_rejected_but_sees_siblings() {
    let mut harness = harness(false, LONG);
    start(&mut harness, owner("a", "owner", &["/r/src/a.rs"], false)).await;
    let backend = parent_backend(&harness);
    tokio::spawn(async move {
        backend
            .spawn(owner("b", "explorer", &[], false), None)
            .await
    });

    let seen = harness
        .requests
        .recv()
        .await
        .expect("undeclared child runs");

    assert_eq!(seen.runtime_overrides.live_siblings.len(), 1);
}

#[tokio::test]
async fn worktree_sibling_is_not_listed_to_shared_child() {
    let mut harness = harness(false, LONG);
    start(&mut harness, owner("a", "isolated", &["/r/a.rs"], true)).await;
    let backend = parent_backend(&harness);
    tokio::spawn(async move {
        backend
            .spawn(owner("b", "shared", &["/r/b.rs"], false), None)
            .await
    });

    let seen = harness.requests.recv().await.expect("shared child runs");

    assert!(seen.runtime_overrides.live_siblings.is_empty());
}

#[tokio::test]
async fn files_are_released_when_owner_finishes() {
    let mut harness = harness(false, LONG);
    let first = start(&mut harness, owner("a", "owner", &["/r/src/a.rs"], false)).await;
    assert!(!rejected_text(&harness, "b").await.is_empty());

    let _ = harness.finish.send(());
    first.await.unwrap().unwrap();

    let backend = parent_backend(&harness);
    tokio::spawn(async move {
        backend
            .spawn(owner("c", "second", &["/r/src/a.rs"], false), None)
            .await
    });
    let seen = harness.requests.recv().await.expect("files free again");
    assert_eq!(seen.id, "c");
}

async fn rejected_text(harness: &Harness, id: &str) -> String {
    rejected(harness, owner(id, "blocked", &["/r/src/a.rs"], false)).await
}

#[tokio::test]
async fn other_root_sessions_do_not_conflict() {
    let mut harness = harness(false, LONG);
    start(&mut harness, owner("a", "owner", &["/r/src/a.rs"], false)).await;
    let mut other = owner("b", "other session", &["/r/src/a.rs"], false);
    other.parent_session_id = "another-parent".to_owned();
    let backend = harness.backend.clone();
    tokio::spawn(async move { backend.spawn(other, None).await });

    let seen = harness
        .requests
        .recv()
        .await
        .expect("other session child runs");

    assert_eq!(seen.id, "b");
    assert!(seen.runtime_overrides.live_siblings.is_empty());
}
