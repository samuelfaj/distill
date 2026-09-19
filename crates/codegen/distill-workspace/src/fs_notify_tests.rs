// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use std::time::Duration;

#[test]
fn workspace_event_kind_round_trip() {
    use distill_workspace_types::FsEventKind as WsKind;
    assert_eq!(
        WsKind::Created,
        to_workspace_event_kind(FsEventKind::Created)
    );
    assert_eq!(
        WsKind::Modified,
        to_workspace_event_kind(FsEventKind::Modified)
    );
    assert_eq!(
        WsKind::Removed,
        to_workspace_event_kind(FsEventKind::Removed)
    );
    assert_eq!(
        WsKind::Renamed,
        to_workspace_event_kind(FsEventKind::Renamed)
    );
}

#[test]
fn parse_diff_name_status_all_variants() {
    use distill_codebase_graph::FileEventKind;
    let root = Path::new("/repo");

    let ev = parse_diff_name_status_line("M\tsrc/main.rs", root).unwrap();
    assert_eq!(FileEventKind::Modified, ev.kind);

    let ev = parse_diff_name_status_line("A\tnew_file.rs", root).unwrap();
    assert_eq!(FileEventKind::Created, ev.kind);

    let ev = parse_diff_name_status_line("D\told_file.rs", root).unwrap();
    assert_eq!(FileEventKind::Removed, ev.kind);

    let ev = parse_diff_name_status_line("R100\told.rs\tnew.rs", root).unwrap();
    assert_eq!(FileEventKind::Renamed, ev.kind);

    assert!(parse_diff_name_status_line("", root).is_none());
}

/// Real OS watcher on a temp root: a file write surfaces as `FsChanged` for that path.
/// The source is armed and subscribed before the write so the event cannot be missed.
#[tokio::test]
async fn file_write_is_broadcast_as_fs_changed() {
    let root = tempfile::tempdir().expect("tempdir");
    let source =
        distill_fsnotify::shared(root.path().to_path_buf(), FsConfig::default()).expect("watcher init");
    let fs_rx = source.subscribe();
    let (events_tx, mut events_rx) = broadcast::channel(16);
    // The watcher reports paths under the canonical directory; the producer strips against that
    // (macOS temp dirs live behind a `/var` → `/private/var` symlink).
    let watch_root = dunce::canonicalize(root.path()).expect("canonical root");
    let forwarder = tokio::spawn(async move {
        forward_fs_changes(fs_rx, &events_tx, &watch_root).await;
    });

    let file = root.path().join("touched.txt");
    std::fs::write(&file, b"x").expect("write");

    let event = tokio::time::timeout(Duration::from_secs(10), events_rx.recv())
        .await
        .expect("FsChanged within timeout")
        .expect("events channel open");
    let WorkspaceEvent::FsChanged { paths, kind } = event else {
        panic!("unexpected event: {event:?}");
    };
    assert_eq!(vec![PathBuf::from("touched.txt")], paths);
    assert!(
        matches!(
            kind,
            distill_workspace_types::FsEventKind::Created
                | distill_workspace_types::FsEventKind::Modified
        ),
        "unexpected kind {kind:?}"
    );
    forwarder.abort();
}

/// A batch past the per-frame bound is split into full frames plus the remainder, in order, so no
/// frame can approach the hub's inbound size cap.
#[tokio::test]
async fn a_batch_past_the_frame_bound_is_split_in_order() {
    let (fs_tx, fs_rx) = broadcast::channel(4);
    let (events_tx, mut events_rx) = broadcast::channel(16);
    let forwarder = tokio::spawn(async move {
        forward_fs_changes(fs_rx, &events_tx, Path::new("/r")).await;
    });
    let absolute: Vec<PathBuf> = (0..FS_CHANGED_PATHS_PER_FRAME + 1)
        .map(|i| PathBuf::from(format!("/r/{i}.rs")))
        .collect();
    let relative: Vec<PathBuf> = (0..FS_CHANGED_PATHS_PER_FRAME + 1)
        .map(|i| PathBuf::from(format!("{i}.rs")))
        .collect();
    fs_tx
        .send(FsEvent::FilesChanged {
            paths: absolute,
            kind: FsEventKind::Created,
        })
        .expect("forwarder subscribed");
    drop(fs_tx);
    forwarder.await.expect("forwarder exits");

    let mut frames = Vec::new();
    while let Ok(WorkspaceEvent::FsChanged { paths, kind }) = events_rx.try_recv() {
        assert_eq!(distill_workspace_types::FsEventKind::Created, kind);
        frames.push(paths);
    }
    assert_eq!(
        vec![FS_CHANGED_PATHS_PER_FRAME, 1],
        frames.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(
        relative,
        frames.concat(),
        "every path once, in watcher order, root-relative"
    );
}

/// One watcher batch is one `FsChanged`: a checkout touching a thousand files is one frame per
/// session, not a thousand.
#[tokio::test]
async fn a_settle_window_of_paths_is_one_fs_changed_frame() {
    let (fs_tx, fs_rx) = broadcast::channel(4);
    let (events_tx, mut events_rx) = broadcast::channel(16);
    let forwarder = tokio::spawn(async move {
        forward_fs_changes(fs_rx, &events_tx, Path::new("/r")).await;
    });
    fs_tx
        .send(FsEvent::FilesChanged {
            paths: vec![PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs")],
            kind: FsEventKind::Modified,
        })
        .expect("forwarder subscribed");
    drop(fs_tx);
    forwarder
        .await
        .expect("forwarder exits when the source closes");

    assert_eq!(
        Ok(WorkspaceEvent::FsChanged {
            paths: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            kind: distill_workspace_types::FsEventKind::Modified,
        }),
        events_rx.try_recv()
    );
    assert_eq!(
        Err(broadcast::error::TryRecvError::Closed),
        events_rx.try_recv(),
        "nothing else was sent for the batch"
    );
}

/// Paths are emitted relative to the watch root and off-root paths are dropped; a batch left
/// empty sends no frame. A rename across the root boundary becomes the half inside it (`Removed`
/// when the file left, `Created` when it arrived) rather than an ambiguous lone-path `Renamed`.
/// Reverting the strip/drop in `forward_fs_changes` fails this.
#[tokio::test]
async fn off_root_paths_are_dropped_and_on_root_paths_are_emitted_relative() {
    use distill_workspace_types::FsEventKind as WireKind;
    let (fs_tx, fs_rx) = broadcast::channel(16);
    let (events_tx, mut events_rx) = broadcast::channel(16);
    let forwarder = tokio::spawn(async move {
        forward_fs_changes(fs_rx, &events_tx, Path::new("/watch")).await;
    });
    let batches = [
        (
            vec![
                "/watch/inside.rs",
                "/elsewhere/outside.rs",
                "/watch/nested/file.rs",
            ],
            FsEventKind::Modified,
        ),
        (
            vec!["/watch/left.rs", "/elsewhere/left.rs"],
            FsEventKind::Renamed,
        ),
        (
            vec!["/elsewhere/arrived.rs", "/watch/arrived.rs"],
            FsEventKind::Renamed,
        ),
        (vec!["/watch/old.rs", "/watch/new.rs"], FsEventKind::Renamed),
        (vec!["/watch/lone.rs"], FsEventKind::Renamed),
        (vec!["/elsewhere/only-off-root.rs"], FsEventKind::Created),
        (
            vec!["/elsewhere/a.rs", "/elsewhere/b.rs"],
            FsEventKind::Renamed,
        ),
    ];
    for (paths, kind) in batches {
        fs_tx
            .send(FsEvent::FilesChanged {
                paths: paths.into_iter().map(PathBuf::from).collect(),
                kind,
            })
            .expect("forwarder subscribed");
    }
    drop(fs_tx);
    forwarder.await.expect("forwarder exits");

    let mut frames = Vec::new();
    while let Ok(WorkspaceEvent::FsChanged { paths, kind }) = events_rx.try_recv() {
        frames.push((paths, kind));
    }
    let relative = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    assert_eq!(
        vec![
            (
                relative(&["inside.rs", "nested/file.rs"]),
                WireKind::Modified
            ),
            (relative(&["left.rs"]), WireKind::Removed),
            (relative(&["arrived.rs"]), WireKind::Created),
            (relative(&["old.rs", "new.rs"]), WireKind::Renamed),
            (relative(&["lone.rs"]), WireKind::Renamed),
        ],
        frames,
        "root-relative; off-root dropped; boundary renames become Removed / Created"
    );
}

/// The watcher reports paths under the real directory even when the root was given through a
/// symlink, and a removed file cannot be canonicalized after the fact. The producer strips against
/// the canonical root it resolved once up front, so a `Removed` under such a root still arrives.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_removal_under_a_symlinked_root_is_emitted_relative() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (real, link) = (dir.path().join("real"), dir.path().join("link"));
    std::fs::create_dir(&real).expect("mkdir");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let (events_tx, mut events_rx) = broadcast::channel(16);
    let _producer = spawn_fs_change_producer(link.clone(), events_tx);
    // The producer arms its watcher on its own task; it holds the shared watcher once armed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let probe = link.clone();
        let holders = tokio::task::spawn_blocking(move || {
            distill_fsnotify::shared(probe, FsConfig::default()).map(|probe| Arc::strong_count(&probe))
        })
        .await
        .expect("probe join")
        .expect("probe watcher");
        if holders == 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "producer never armed its watcher"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    async fn next(
        events_rx: &mut broadcast::Receiver<WorkspaceEvent>,
    ) -> (Vec<PathBuf>, distill_workspace_types::FsEventKind) {
        match tokio::time::timeout(Duration::from_secs(10), events_rx.recv())
            .await
            .expect("FsChanged within timeout")
            .expect("events channel open")
        {
            WorkspaceEvent::FsChanged { paths, kind } => (paths, kind),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    let file = link.join("gone.txt");
    std::fs::write(&file, b"x").expect("write");
    let (paths, kind) = next(&mut events_rx).await;
    assert_eq!(vec![PathBuf::from("gone.txt")], paths, "written: {kind:?}");

    std::fs::remove_file(&file).expect("remove");
    assert_eq!(
        (
            vec![PathBuf::from("gone.txt")],
            distill_workspace_types::FsEventKind::Removed
        ),
        next(&mut events_rx).await
    );
}

/// A real, cache-less index over `root` (created if missing).
fn index_over(root: &Path) -> Arc<distill_codebase_graph::IndexManagerHandle> {
    use distill_codebase_graph::{IndexManager, IndexManagerConfig};
    std::fs::create_dir_all(root).expect("mkdir");
    let mut config = IndexManagerConfig::new(root.to_path_buf());
    config.load_from_cache = false;
    config.save_to_cache = false;
    IndexManager::spawn(config)
}

/// Where `idx` defines `symbol`, as paths relative to its root. Commands are applied in order, so
/// this sees every event sent before it.
fn definition_of(idx: &distill_codebase_graph::IndexManagerHandle, symbol: &str) -> Vec<String> {
    idx.find_definitions_blocking(symbol.to_owned(), None)
        .expect("index alive")
        .into_iter()
        .map(|location| location.path)
        .collect()
}

/// A `Renamed` pair whose halves share an index reaches it as one `FileEvent` carrying both paths
/// (the graph decomposes it into Removed(from) + Created(to) only then); any other batch is one
/// event per covering index, each holding that index's paths, uncovered paths dropped.
#[test]
fn a_batch_reaches_each_index_as_one_file_event() {
    use distill_codebase_graph::FileEventKind;
    let root = tempfile::tempdir().expect("tempdir");
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    let (idx_a, idx_b) = (index_over(&a), index_over(&b));
    let covering = |path: &Path| {
        if path.starts_with(&a) {
            Some(idx_a.clone())
        } else if path.starts_with(&b) {
            Some(idx_b.clone())
        } else {
            None
        }
    };

    let (from, to) = (a.join("old.rs"), a.join("new.rs"));
    let renamed = codebase_graph_events_for_batch(
        vec![from.clone(), to.clone()],
        distill_workspace_types::FsEventKind::Renamed,
        covering,
    );
    let [(idx, event)] = renamed.as_slice() else {
        panic!(
            "one event for the pair, got {:?}",
            renamed.iter().map(|(_, e)| e).collect::<Vec<_>>()
        );
    };
    assert!(Arc::ptr_eq(idx, &idx_a));
    assert_eq!(FileEventKind::Renamed, event.kind);
    assert_eq!(vec![from, to], event.paths, "both paths, from first");

    let created = codebase_graph_events_for_batch(
        vec![
            a.join("1.rs"),
            b.join("1.rs"),
            a.join("2.rs"),
            root.path().join("outside.rs"),
        ],
        distill_workspace_types::FsEventKind::Created,
        covering,
    );
    let mut per_index: Vec<(bool, Vec<PathBuf>)> = created
        .iter()
        .map(|(idx, event)| (Arc::ptr_eq(idx, &idx_a), event.paths.clone()))
        .collect();
    per_index.sort_by_key(|(is_a, _)| !is_a);
    assert_eq!(
        vec![
            (true, vec![a.join("1.rs"), a.join("2.rs")]),
            (false, vec![b.join("1.rs")]),
        ],
        per_index,
        "grouped per index, in batch order, the uncovered path dropped"
    );
    assert!(
        created
            .iter()
            .all(|(_, e)| e.kind == FileEventKind::Created)
    );
}

/// A rename across index roots, driven through two real indexes: `a/old.rs` (defining
/// `moved_symbol_xyz`) becomes `b/new.rs`. Each half goes to its own index as its own event, so A
/// forgets the symbol and B learns it. Sent whole to A, A would index a file outside its root under
/// an absolute key and B would never hear of the file.
#[test]
fn a_rename_across_index_roots_moves_the_symbol_between_the_indexes() {
    use distill_codebase_graph::FileEventKind;
    let root = tempfile::tempdir().expect("tempdir");
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    let (idx_a, idx_b) = (index_over(&a), index_over(&b));
    let covering = |path: &Path| {
        if path.starts_with(&a) {
            Some(idx_a.clone())
        } else if path.starts_with(&b) {
            Some(idx_b.clone())
        } else {
            None
        }
    };
    let (from, to) = (a.join("old.rs"), b.join("new.rs"));
    std::fs::write(&from, "fn moved_symbol_xyz() {}\n").expect("write");
    for (idx, event) in codebase_graph_events_for_batch(
        vec![from.clone()],
        distill_workspace_types::FsEventKind::Created,
        covering,
    ) {
        idx.send_event(event).expect("index alive");
    }
    assert_eq!(vec!["old.rs"], definition_of(&idx_a, "moved_symbol_xyz"));

    std::fs::rename(&from, &to).expect("rename");
    let events = codebase_graph_events_for_batch(
        vec![from.clone(), to.clone()],
        distill_workspace_types::FsEventKind::Renamed,
        covering,
    );
    let routed: Vec<(bool, FileEventKind, Vec<PathBuf>)> = events
        .iter()
        .map(|(idx, e)| (Arc::ptr_eq(idx, &idx_a), e.kind, e.paths.clone()))
        .collect();
    assert_eq!(
        vec![
            (true, FileEventKind::Removed, vec![from.clone()]),
            (false, FileEventKind::Created, vec![to.clone()]),
        ],
        routed,
        "each half to its own index, as its own event"
    );
    for (idx, event) in events {
        idx.send_event(event).expect("index alive");
    }
    assert!(
        definition_of(&idx_a, "moved_symbol_xyz").is_empty(),
        "A forgot the symbol"
    );
    assert_eq!(
        vec!["new.rs"],
        definition_of(&idx_b, "moved_symbol_xyz"),
        "B learned it under the new path"
    );
}

/// Nested roots, the shape `get_covering` routes by longest prefix: R over `r/`, S over `r/sub/`.
/// `r/old.rs` → `r/sub/new.rs` lands on S as a Created, so the index every query for that file is
/// routed to is the one that knows it.
#[test]
fn a_rename_into_a_nested_root_reaches_the_inner_index() {
    use distill_codebase_graph::FileEventKind;
    let root = tempfile::tempdir().expect("tempdir");
    let (r, sub) = (root.path().join("r"), root.path().join("r").join("sub"));
    let (idx_r, idx_s) = (index_over(&r), index_over(&sub));
    let covering = |path: &Path| {
        if path.starts_with(&sub) {
            Some(idx_s.clone())
        } else if path.starts_with(&r) {
            Some(idx_r.clone())
        } else {
            None
        }
    };
    let (from, to) = (r.join("old.rs"), sub.join("new.rs"));
    std::fs::write(&from, "fn nested_symbol_xyz() {}\n").expect("write");
    std::fs::rename(&from, &to).expect("rename");
    let events = codebase_graph_events_for_batch(
        vec![from.clone(), to.clone()],
        distill_workspace_types::FsEventKind::Renamed,
        covering,
    );
    assert_eq!(
        vec![
            (true, FileEventKind::Removed, vec![from]),
            (false, FileEventKind::Created, vec![to.clone()]),
        ],
        events
            .iter()
            .map(|(idx, e)| (Arc::ptr_eq(idx, &idx_r), e.kind, e.paths.clone()))
            .collect::<Vec<_>>()
    );
    for (idx, event) in events {
        idx.send_event(event).expect("index alive");
    }
    assert_eq!(vec!["new.rs"], definition_of(&idx_s, "nested_symbol_xyz"));

    // A rename the watcher reported by one path is a `Renamed` its index re-indexes
    let lone = codebase_graph_events_for_batch(
        vec![sub.join("new.rs")],
        distill_workspace_types::FsEventKind::Renamed,
        covering,
    );
    let [(idx, event)] = lone.as_slice() else {
        panic!("one event for a lone path, got {}", lone.len());
    };
    assert!(Arc::ptr_eq(idx, &idx_s));
    assert_eq!(FileEventKind::Renamed, event.kind);
    assert_eq!(vec![sub.join("new.rs")], event.paths);
}

/// The producer's handle owns the watch: while it lives a fresh `shared()` on the root joins the
/// producer's watcher (two holders); once it is dropped — no `shutdown`, just the drop — the task
/// is aborted and a fresh `shared()` is the only holder again.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_producer_handle_ends_the_task_and_releases_the_watcher() {
    let root = tempfile::tempdir().expect("tempdir");
    let (events_tx, _events_rx) = broadcast::channel(16);
    let producer = spawn_fs_change_producer(root.path().to_path_buf(), events_tx);

    let holders = |root: PathBuf| async move {
        tokio::task::spawn_blocking(move || {
            distill_fsnotify::shared(root, FsConfig::default())
                .map(|probe| Arc::strong_count(&probe))
                .expect("probe watcher")
        })
        .await
        .expect("probe join")
    };
    let settles_at = |want: usize| {
        let root = root.path().to_path_buf();
        async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let seen = holders(root.clone()).await;
                if seen == want {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "watcher holders stayed at {seen}, wanted {want}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };

    settles_at(2).await;
    drop(producer);
    settles_at(1).await;
}
