// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use std::path::PathBuf;
use distill_tracing_macros::teprintln;

#[test]
fn test_map_event_kind() {
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    assert_eq!(
        map_event_kind(&EventKind::Create(CreateKind::File)),
        Some(FsEventKind::Created)
    );
    assert_eq!(
        map_event_kind(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Content
        ))),
        Some(FsEventKind::Modified)
    );
    assert_eq!(
        map_event_kind(&EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Both
        ))),
        Some(FsEventKind::Renamed)
    );
    assert_eq!(
        map_event_kind(&EventKind::Remove(RemoveKind::File)),
        Some(FsEventKind::Removed)
    );
    assert_eq!(map_event_kind(&EventKind::Other), None);
}

// Integration tests with a real filesystem and debouncer.
// Serialized because macOS FSEvents has limited resources when many watchers are created at once.

mod integration {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Default debounce time for tests
    const TEST_DEBOUNCE_MS: u64 = 50;
    /// Max time to wait for events after debounce (debounce + buffer)
    const EVENT_WAIT_MS: u64 = 300;
    /// Timeout for watcher initialization in tests
    const TEST_INIT_TIMEOUT: Duration = Duration::from_secs(15);
    /// Number of retries for starting watcher (helps with flaky FSEvents)
    const START_RETRIES: usize = 3;

    /// Start a watcher with retry logic for flaky FSEvents.
    fn start_with_retry(
        watch_path: PathBuf,
        config: FsNotifyConfig,
    ) -> Result<
        (
            tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
            FsNotifyHandle,
        ),
        crate::FsNotifyError,
    > {
        start_with_retry_strategy(watch_path, config, watch_strategy())
    }

    /// Like [`start_with_retry`] but with an explicit strategy, so tests
    /// can exercise both layouts without process-global env races.
    fn start_with_retry_strategy(
        watch_path: PathBuf,
        config: FsNotifyConfig,
        strategy: WatchStrategy,
    ) -> Result<
        (
            tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
            FsNotifyHandle,
        ),
        crate::FsNotifyError,
    > {
        let mut last_error = None;
        for attempt in 1..=START_RETRIES {
            match start_with_timeout(
                watch_path.clone(),
                config.clone(),
                true,
                strategy,
                TEST_INIT_TIMEOUT,
            ) {
                Ok(result) => return Ok(result),
                Err(e) => {
                    teprintln!(
                        "Watcher start attempt {}/{} failed: {}",
                        attempt,
                        START_RETRIES,
                        e
                    );
                    last_error = Some(e);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            crate::FsNotifyError::WatcherStart(
                std::io::Error::other("failed to start watcher").into(),
            )
        }))
    }

    /// Helper to collect events with timeout, with early exit once we get events
    /// and a quiet period passes without new ones.
    fn collect_events_smart(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
        max_wait: Duration,
        quiet_period: Duration,
    ) -> Vec<RawFsEvent> {
        let mut events = Vec::new();
        let deadline = std::time::Instant::now() + max_wait;
        let mut last_event_time = std::time::Instant::now();

        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(event) => {
                    events.push(event);
                    last_event_time = std::time::Instant::now();
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    // If we have events and quiet period elapsed, return early
                    if !events.is_empty() && last_event_time.elapsed() >= quiet_period {
                        return events;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    /// Collect events with default timing
    fn collect_events(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
    ) -> Vec<RawFsEvent> {
        collect_events_smart(
            rx,
            Duration::from_millis(EVENT_WAIT_MS),
            Duration::from_millis(50),
        )
    }

    #[test]
    #[serial]
    fn test_handle_drop_stops_watcher() {
        let temp_dir = TempDir::new().unwrap();
        let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

        let config = FsNotifyConfig {
            debounce_ms: TEST_DEBOUNCE_MS,
            ..Default::default()
        };

        let (mut rx, handle) = start_with_retry(watch_path.clone(), config).unwrap();
        let _ = collect_events(&mut rx); // drain startup stragglers

        // Drop joins the watcher thread, which drops the debouncer and the event sender.
        // Run it on a watchdog thread so a hung join fails fast instead of hanging CI.
        let dropper = std::thread::spawn(move || drop(handle));
        let drop_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dropper.is_finished() {
            assert!(
                std::time::Instant::now() < drop_deadline,
                "FsNotifyHandle::drop did not return within 5s — shutdown is broken"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        dropper.join().unwrap();

        // The receiver must observe disconnection within a bounded time —
        // this is what proves the watcher actually stopped.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let disconnected = loop {
            match rx.try_recv() {
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break true,
                Ok(_) => {} // drain any straggler before disconnect
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert!(
            disconnected,
            "event channel must disconnect after the handle is dropped"
        );

        // And a post-drop write must not surface (watcher is gone) — this
        // would still arrive if shutdown had not torn down the watch.
        fs::write(watch_path.join("after_drop.txt"), "test").unwrap();
        std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
        let after = collect_events_smart(
            &mut rx,
            Duration::from_millis(100),
            Duration::from_millis(20),
        );
        assert!(
            !after
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("after_drop.txt"))),
            "no event should surface after the watcher is dropped, got: {after:?}"
        );
    }

    // ── per-dir strategy (Linux default; forced here so it runs on any
    //    platform without process-global env races) ──────────────────────

    /// A checkout that appears while the session runs is not armed.
    #[test]
    #[serial]
    fn worktree_created_mid_session_is_not_watched() {
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let worktree = root.join("added-worktree");
        fs::create_dir_all(worktree.join("crates/core")).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: /repo/.git/worktrees/added\n",
        )
        .unwrap();
        let plain = root.join("added-source");
        fs::create_dir_all(plain.join("nested")).unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<RawFsEvent>();
        let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
            Duration::from_millis(TEST_DEBOUNCE_MS),
            None,
            |_: DebounceEventResult| {},
            NoCache,
            notify::Config::default().with_follow_symlinks(false),
        )
        .unwrap();
        let mut watched = HashSet::new();

        add_subtree_watches(
            &mut debouncer,
            &mut watched,
            &root,
            &worktree,
            /*custom_ignore*/ &None,
            /*custom_include*/ &None,
            /*budget*/ 1024,
            &tx,
        );
        assert!(watched.is_empty(), "a new worktree must not be watched");

        // The marker and the checkout's own directories can share one
        // debounce batch, so a child is queued as an add of its own.
        add_subtree_watches(
            &mut debouncer,
            &mut watched,
            &root,
            &worktree.join("crates"),
            /*custom_ignore*/ &None,
            /*custom_include*/ &None,
            /*budget*/ 1024,
            &tx,
        );
        assert!(watched.is_empty(), "nor may anything inside it");

        add_subtree_watches(
            &mut debouncer,
            &mut watched,
            &root,
            &plain,
            /*custom_ignore*/ &None,
            /*custom_include*/ &None,
            /*budget*/ 1024,
            &tx,
        );
        assert_eq!(
            watched.len(),
            2,
            "ordinary new directories are still watched"
        );
    }

    /// Worktrees parked inside a project cost no watches of their own.
    #[test]
    #[serial]
    fn project_dir_nested_worktrees_cost_no_extra_watches() {
        const HIDDEN_PARENT: &str = ".harness/worktrees";
        const PLAIN_PARENT: &str = "worktrees";
        const PER_PARENT: usize = 12;

        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::create_dir_all(root.join("crates/core")).unwrap();

        let add_worktree = |parent: &str, index: usize| {
            let name = format!("{}-{index}", parent.replace('/', "-"));
            let worktree = root.join(parent).join(&name);
            fs::create_dir_all(worktree.join("crates/core/src")).unwrap();
            let git_dir = root.join(".git/worktrees").join(&name);
            fs::create_dir_all(&git_dir).unwrap();
            fs::write(
                worktree.join(".git"),
                format!("gitdir: {}\n", git_dir.display()),
            )
            .unwrap();
        };
        for index in 0..PER_PARENT {
            add_worktree(HIDDEN_PARENT, index);
            add_worktree(PLAIN_PARENT, index);
        }

        let (_rx, handle) = start_with_retry_strategy(
            root.clone(),
            FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            },
            WatchStrategy::PerDir,
        )
        .unwrap();

        // root(1) + crates, crates/core(2) + .harness, .harness/worktrees(2)
        // + worktrees(1) + .git, .git/refs, .git/refs/heads(3).
        const EXPECTED_WATCHES: usize = 9;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.watch_count() != EXPECTED_WATCHES && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            handle.watch_count(),
            EXPECTED_WATCHES,
            "{} nested worktrees must not enlarge the watch set",
            PER_PARENT * 2
        );
    }

    /// Watch-count accounting: nested gitignored dirs cost zero watches
    /// and `.git` costs a handful, not one per internal dir.
    #[test]
    #[serial]
    fn test_per_dir_watch_count_excludes_ignored_and_git_internals() {
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(root.join("web/src")).unwrap();
        for i in 0..20 {
            fs::create_dir_all(root.join(format!("web/node_modules/pkg{i}/lib"))).unwrap();
        }

        let (_rx, handle) = start_with_retry_strategy(
            root.clone(),
            FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            },
            WatchStrategy::PerDir,
        )
        .unwrap();

        // root(1) + web + web/src (2) + .git,.git/refs,.git/refs/heads (3).
        // The 40 node_modules dirs and .git/objects contribute nothing.
        // Depth≥2 dirs arm asynchronously after `ready`, so poll briefly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.watch_count() != 6 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            handle.watch_count(),
            6,
            "per-dir watch layout must skip ignored trees and git internals"
        );
    }
}

// ========================================================================
// Unit tests for merge_events and build_globsets
// ========================================================================

mod merge_events_tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

    fn make_debounced_event(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        DebouncedEvent {
            event: notify::Event {
                kind,
                paths,
                attrs: Default::default(),
            },
            time: std::time::Instant::now(),
        }
    }

    #[test]
    fn test_merge_single_create() {
        let events = vec![make_debounced_event(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/test/file.txt")],
        )];

        let merged = merge_events(events);

        assert_eq!(merged.len(), 1);
        let Some(first) = merged.first() else {
            panic!("expected one merged event: {merged:?}");
        };
        assert_eq!(first.kind, FsEventKind::Created);
        assert_eq!(first.paths.len(), 1);
        assert_eq!(first.paths.first(), Some(&PathBuf::from("/test/file.txt")));
    }

    #[test]
    fn test_merge_multiple_creates_same_path() {
        // Multiple creates for same path should result in single create
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
        ];

        let merged = merge_events(events);

        // Should be merged into one event
        let create_count: usize = merged
            .iter()
            .filter(|e| e.kind == FsEventKind::Created)
            .map(|e| e.paths.len())
            .sum();
        assert_eq!(create_count, 1, "Duplicate creates should be merged");
    }

    #[test]
    fn test_merge_create_then_modify() {
        // Create followed by modify should remain Create
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                vec![PathBuf::from("/test/file.txt")],
            ),
        ];

        let merged = merge_events(events);

        let path_kinds: std::collections::HashMap<_, _> = merged
            .iter()
            .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
            .collect();

        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/file.txt")),
            Some(&FsEventKind::Created),
            "Create+Modify should remain Create"
        );
    }

    #[test]
    fn test_merge_modify_then_create() {
        // Modify followed by create should become Create
        let events = vec![
            make_debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
        ];

        let merged = merge_events(events);

        let path_kinds: std::collections::HashMap<_, _> = merged
            .iter()
            .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
            .collect();

        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/file.txt")),
            Some(&FsEventKind::Created),
            "Modify+Create should become Create"
        );
    }

    #[test]
    fn test_merge_any_then_remove() {
        // Any event followed by remove should become Remove
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Remove(RemoveKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
        ];

        let merged = merge_events(events);

        let path_kinds: std::collections::HashMap<_, _> = merged
            .iter()
            .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
            .collect();

        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/file.txt")),
            Some(&FsEventKind::Removed),
            "Any+Remove should become Remove"
        );
    }

    #[test]
    fn test_merge_any_then_rename() {
        // Any event followed by rename should become Rename
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            ),
            make_debounced_event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                vec![PathBuf::from("/test/file.txt")],
            ),
        ];

        let merged = merge_events(events);

        let path_kinds: std::collections::HashMap<_, _> = merged
            .iter()
            .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
            .collect();

        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/file.txt")),
            Some(&FsEventKind::Renamed),
            "Any+Rename should become Rename"
        );
    }

    #[test]
    fn test_merge_other_events_filtered() {
        // Other/Access events should be filtered out
        let events = vec![
            make_debounced_event(EventKind::Other, vec![PathBuf::from("/test/other.txt")]),
            make_debounced_event(
                EventKind::Access(notify::event::AccessKind::Read),
                vec![PathBuf::from("/test/access.txt")],
            ),
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/create.txt")],
            ),
        ];

        let merged = merge_events(events);

        // Should only have the Create event
        assert_eq!(merged.len(), 1);
        let Some(first) = merged.first() else {
            panic!("expected one merged event: {merged:?}");
        };
        assert_eq!(first.kind, FsEventKind::Created);
        assert!(first.paths.iter().any(|p| p.ends_with("create.txt")));
    }

    #[test]
    fn test_merge_multiple_different_paths() {
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/a.txt")],
            ),
            make_debounced_event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                vec![PathBuf::from("/test/b.txt")],
            ),
            make_debounced_event(
                EventKind::Remove(RemoveKind::File),
                vec![PathBuf::from("/test/c.txt")],
            ),
        ];

        let merged = merge_events(events);

        let path_kinds: std::collections::HashMap<_, _> = merged
            .iter()
            .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
            .collect();

        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/a.txt")),
            Some(&FsEventKind::Created)
        );
        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/b.txt")),
            Some(&FsEventKind::Modified)
        );
        assert_eq!(
            path_kinds.get(&PathBuf::from("/test/c.txt")),
            Some(&FsEventKind::Removed)
        );
    }

    #[test]
    fn test_merge_empty_events() {
        let events: Vec<DebouncedEvent> = vec![];
        let merged = merge_events(events);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_merge_groups_by_kind() {
        // Multiple files with same kind should be grouped
        let events = vec![
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/a.txt")],
            ),
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/b.txt")],
            ),
            make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/c.txt")],
            ),
        ];

        let merged = merge_events(events);

        // All creates should be grouped into one RawFsEvent
        let create_events: Vec<_> = merged
            .iter()
            .filter(|e| e.kind == FsEventKind::Created)
            .collect();
        assert_eq!(create_events.len(), 1);
        assert_eq!(create_events.first().map(|e| e.paths.len()), Some(3));
    }
}

mod build_globsets_tests {
    use super::*;

    #[test]
    fn test_build_globsets_empty() {
        let (ignore, include) = build_globsets(&[]);
        assert!(ignore.is_none());
        assert!(include.is_none());
    }

    #[test]
    fn test_build_globsets_ignore_pattern() {
        let (ignore, include) = build_globsets(&["*.log".to_string()]);

        assert!(ignore.is_some());
        assert!(include.is_none());

        let ignore_set = ignore.unwrap();
        assert!(ignore_set.is_match("debug.log"));
        assert!(ignore_set.is_match("path/to/error.log"));
        assert!(!ignore_set.is_match("readme.txt"));
    }

    #[test]
    fn test_build_globsets_negation_pattern() {
        let (ignore, include) = build_globsets(&["!important.log".to_string()]);

        assert!(ignore.is_none());
        assert!(include.is_some());

        let include_set = include.unwrap();
        assert!(include_set.is_match("important.log"));
        assert!(include_set.is_match("path/to/important.log"));
        assert!(!include_set.is_match("other.log"));
    }

    #[test]
    fn test_build_globsets_mixed_patterns() {
        let (ignore, include) = build_globsets(&[
            "*.log".to_string(),
            "*.tmp".to_string(),
            "!important.log".to_string(),
            "!keep.tmp".to_string(),
        ]);

        assert!(ignore.is_some());
        assert!(include.is_some());

        let ignore_set = ignore.unwrap();
        let include_set = include.unwrap();

        // Ignore patterns
        assert!(ignore_set.is_match("debug.log"));
        assert!(ignore_set.is_match("cache.tmp"));

        // Include patterns (negations)
        assert!(include_set.is_match("important.log"));
        assert!(include_set.is_match("keep.tmp"));
    }

    #[test]
    fn test_build_globsets_directory_pattern() {
        let (ignore, _) = build_globsets(&["target/**".to_string()]);

        assert!(ignore.is_some());
        let ignore_set = ignore.unwrap();

        assert!(ignore_set.is_match("target/debug/binary"));
        assert!(ignore_set.is_match("target/release/lib.so"));
        assert!(!ignore_set.is_match("src/main.rs"));
    }

    #[test]
    fn test_build_globsets_absolute_pattern() {
        // Patterns starting with / shouldn't get **/ prepended
        let (ignore, _) = build_globsets(&["/root_only.txt".to_string()]);

        assert!(ignore.is_some());
        let ignore_set = ignore.unwrap();

        // Note: The pattern /root_only.txt matches paths ending with /root_only.txt
        // This is slightly different from gitignore semantics but works for our use case
        assert!(ignore_set.is_match("/root_only.txt"));
    }

    #[test]
    fn test_build_globsets_already_prefixed() {
        // Patterns already starting with **/ shouldn't get double-prefixed
        let (ignore, _) = build_globsets(&["**/node_modules/**".to_string()]);

        assert!(ignore.is_some());
        let ignore_set = ignore.unwrap();

        assert!(ignore_set.is_match("node_modules/package/index.js"));
        assert!(ignore_set.is_match("frontend/node_modules/lodash/index.js"));
    }

    #[test]
    fn test_build_globsets_complex_patterns() {
        let (ignore, _) = build_globsets(&[
            "*.{log,tmp,bak}".to_string(),
            "__pycache__/**".to_string(),
            ".DS_Store".to_string(),
        ]);

        assert!(ignore.is_some());
        let ignore_set = ignore.unwrap();

        assert!(ignore_set.is_match("debug.log"));
        assert!(ignore_set.is_match("file.tmp"));
        assert!(ignore_set.is_match("backup.bak"));
        assert!(ignore_set.is_match("__pycache__/module.pyc"));
        assert!(ignore_set.is_match(".DS_Store"));
        assert!(ignore_set.is_match("subdir/.DS_Store"));
    }
}

mod config_tests {
    use super::*;

    #[test]
    fn is_git_path_for_watcher_lets_index_lock_through() {
        assert!(is_git_path_for_watcher(Path::new("/r/.git/index.lock")));
        assert!(is_git_path_for_watcher(Path::new("/r/.git/HEAD")));
        assert!(!is_git_path_for_watcher(Path::new(
            "/r/.git/COMMIT_EDITMSG"
        )));
        assert!(!is_git_path_for_watcher(Path::new("/r/src/main.rs")));
    }

    #[test]
    fn is_sl_path_for_watcher_lets_only_wlock_through() {
        assert!(is_sl_path_for_watcher(Path::new("/r/.sl/wlock")));
        // dirstate is read on demand, never watched — must NOT pass.
        assert!(!is_sl_path_for_watcher(Path::new("/r/.sl/dirstate")));
        assert!(!is_sl_path_for_watcher(Path::new("/r/.sl/store/lock")));
        assert!(!is_sl_path_for_watcher(Path::new("/r/src/main.rs")));
    }

    #[test]
    fn gitignore_cache_is_ignored_handles_sl_like_git() {
        let mut cache = GitignoreCache::default();
        // Only `.sl/wlock` reaches the source; everything else under `.sl`
        // (notably `dirstate`, read on demand) stays ignored.
        assert!(!cache.is_ignored(Path::new("/ws/.sl/wlock"), true, true));
        assert!(cache.is_ignored(Path::new("/ws/.sl/dirstate"), true, true));
        assert!(cache.is_ignored(Path::new("/ws/.sl/store/lock"), true, true));
        // With watch_vcs off, even wlock is ignored (mirrors `.git`).
        assert!(cache.is_ignored(Path::new("/ws/.sl/wlock"), false, true));
        // Kill-switch off: the `.sl` arm is skipped, so `.sl/*` is no longer
        // specially ignored here (it is dropped structurally in the source).
        assert!(!cache.is_ignored(Path::new("/ws/.sl/dirstate"), true, false));
    }

    #[test]
    fn test_gitignore_cache_is_ignored_with_watch_vcs() {
        let mut cache = GitignoreCache::default();

        // Without watch_vcs, all git paths are ignored.
        assert!(cache.is_ignored(Path::new("/workspace/.git/index"), false, true));
        assert!(cache.is_ignored(Path::new("/workspace/.git/HEAD"), false, true));
        assert!(cache.is_ignored(Path::new("/workspace/.git/objects/123"), false, true));

        // With watch_vcs, watched git files are NOT ignored.
        assert!(!cache.is_ignored(Path::new("/workspace/.git/index"), true, true));
        assert!(!cache.is_ignored(Path::new("/workspace/.git/HEAD"), true, true));
        assert!(!cache.is_ignored(Path::new("/workspace/.git/refs/heads/main"), true, true));
        assert!(!cache.is_ignored(Path::new("/workspace/.git/packed-refs"), true, true));

        // Other git paths are still ignored even with watch_vcs.
        assert!(cache.is_ignored(Path::new("/workspace/.git/objects/123"), true, true));
        assert!(cache.is_ignored(Path::new("/workspace/.git/COMMIT_EDITMSG"), true, true));
    }
}

mod select_top_level_watch_dirs_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn contains_name(dirs: &[PathBuf], name: &str) -> bool {
        dirs.iter()
            .any(|d| d.file_name().is_some_and(|n| n == name))
    }

    #[test]
    fn returns_only_immediate_child_dirs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src/utils")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("README.md"), "x").unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"), "src must be watched: {dirs:?}");
        assert!(
            contains_name(&dirs, "tests"),
            "tests must be watched: {dirs:?}"
        );
        // Depth-2 dirs are reached via the child's recursive watch, not here.
        assert!(
            !contains_name(&dirs, "utils"),
            "nested utils must not be a top-level entry: {dirs:?}"
        );
        // The root is watched non-recursively and never returned here.
        assert!(!dirs.iter().any(|d| d == root), "root must not be returned");
        // Files are covered by the root watch, not watched directly.
        assert!(!contains_name(&dirs, "README.md"));
    }

    #[test]
    fn excludes_git_directory() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"));
        assert!(
            !contains_name(&dirs, ".git"),
            ".git is watched separately, not as a recursive child: {dirs:?}"
        );
    }

    #[test]
    fn excludes_top_level_checkouts() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("vendored/.git")).unwrap();
        let worktree = root.join("feature");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /repo/.git/worktrees/x\n").unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"));
        assert!(
            !contains_name(&dirs, "vendored"),
            "a nested clone belongs to another workspace: {dirs:?}"
        );
        assert!(
            !contains_name(&dirs, "feature"),
            "a linked worktree belongs to another workspace: {dirs:?}"
        );
    }

    #[test]
    fn excludes_sl_directory() {
        // `.sl` is watched separately (non-recursively), never as a
        // recursive workspace child — same treatment as `.git`.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".sl/store")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"));
        assert!(
            !contains_name(&dirs, ".sl"),
            ".sl must not be a recursive child (avoids .sl/store churn): {dirs:?}"
        );
    }

    #[test]
    fn excludes_gitignored_target_but_keeps_similar_names() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        // `.git` is required for WalkBuilder to honor `.gitignore`.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("target_data")).unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"), "src must be watched: {dirs:?}");
        // The whole point of the change: a gitignored top-level dir is
        // never watched, so its churn never reaches the pipeline.
        assert!(
            !contains_name(&dirs, "target"),
            "gitignored target must be excluded: {dirs:?}"
        );
        // A different dir that merely shares a prefix must not be excluded.
        assert!(
            contains_name(&dirs, "target_data"),
            "non-ignored target_data must be watched: {dirs:?}"
        );
    }

    #[test]
    fn honors_custom_ignore_patterns() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();

        let (ignore, _) = build_globsets(&["**/vendor".to_string()]);
        let dirs = select_top_level_watch_dirs(root, &ignore, &None);

        assert!(contains_name(&dirs, "src"));
        assert!(
            !contains_name(&dirs, "vendor"),
            "custom-ignored vendor must be excluded: {dirs:?}"
        );
    }

    #[test]
    fn custom_include_overrides_custom_ignore() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();

        let (ignore, include) =
            build_globsets(&["**/vendor".to_string(), "!**/vendor".to_string()]);

        // Control: ignore alone excludes vendor.
        let ignored_only = select_top_level_watch_dirs(root, &ignore, &None);
        assert!(
            !contains_name(&ignored_only, "vendor"),
            "control: ignore alone must exclude vendor: {ignored_only:?}"
        );

        // Include overrides the ignore for the same path.
        let dirs = select_top_level_watch_dirs(root, &ignore, &include);
        assert!(
            contains_name(&dirs, "vendor"),
            "include must override ignore: {dirs:?}"
        );
        assert!(contains_name(&dirs, "src"));
    }

    #[test]
    fn empty_root_returns_no_dirs() {
        let temp = TempDir::new().unwrap();
        let dirs = select_top_level_watch_dirs(temp.path(), &None, &None);
        assert!(dirs.is_empty(), "empty root has no child dirs: {dirs:?}");
    }

    #[test]
    fn files_only_returns_no_dirs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("a.txt"), "x").unwrap();
        fs::write(root.join("b.txt"), "x").unwrap();
        let dirs = select_top_level_watch_dirs(root, &None, &None);
        assert!(
            dirs.is_empty(),
            "top-level files are covered by the root watch: {dirs:?}"
        );
    }

    #[test]
    fn hidden_non_ignored_dir_is_included() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".config")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let dirs = select_top_level_watch_dirs(root, &None, &None);
        assert!(
            contains_name(&dirs, ".config"),
            "hidden non-ignored dirs are watched: {dirs:?}"
        );
        assert!(contains_name(&dirs, "src"));
    }

    #[test]
    fn watches_children_even_when_root_is_under_a_gitignored_path() {
        // The user explicitly chose a cwd that an ancestor .gitignore marks
        // ignored; its children must still be watched.
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        let watch_root = repo.join("build");
        fs::create_dir_all(watch_root.join("src")).unwrap();
        fs::create_dir_all(watch_root.join("out")).unwrap();

        let dirs = select_top_level_watch_dirs(&watch_root, &None, &None);

        assert!(
            contains_name(&dirs, "src"),
            "children of a gitignored root must still be watched: {dirs:?}"
        );
        assert!(
            contains_name(&dirs, "out"),
            "children of a gitignored root must still be watched: {dirs:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_child_dir_is_skipped() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("real")).unwrap();
        // A symlinked dir would, if recursively watched, leave the
        // workspace; it must be skipped.
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "real"), "real dir watched: {dirs:?}");
        assert!(
            !contains_name(&dirs, "link"),
            "symlinked child must be skipped: {dirs:?}"
        );
    }

    #[test]
    fn excludes_dir_ignored_only_by_git_info_exclude() {
        // `.git/info/exclude` is honored at the watch level (WalkBuilder)
        // but NOT by the per-event GitignoreCache — so this exercises the
        // stronger watch-level coverage specifically.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".git/info/exclude"), "generated/\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("generated")).unwrap();

        let dirs = select_top_level_watch_dirs(root, &None, &None);

        assert!(contains_name(&dirs, "src"), "src watched: {dirs:?}");
        assert!(
            !contains_name(&dirs, "generated"),
            ".git/info/exclude'd dir must be excluded at the watch level: {dirs:?}"
        );
    }

    #[test]
    fn gitignore_wins_over_custom_include_at_watch_level() {
        // WalkBuilder never yields a gitignored child, so a negation cannot
        // re-add a gitignored top-level dir at the watch level.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let (_, include) = build_globsets(&["!**/vendor".to_string()]);
        let dirs = select_top_level_watch_dirs(root, &None, &include);

        assert!(contains_name(&dirs, "src"));
        assert!(
            !contains_name(&dirs, "vendor"),
            "gitignore wins over custom_include at the watch level: {dirs:?}"
        );
    }
}

mod per_dir_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn rel(dirs: &[PathBuf], root: &Path) -> Vec<String> {
        dirs.iter()
            .map(|d| {
                d.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    /// The core regression: gitignored dirs nested *below* the top level
    /// (invisible to the fan-out selector) are pruned at every depth.
    #[test]
    fn select_per_dir_prunes_nested_gitignored_dirs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
        fs::create_dir_all(root.join("web/src/components")).unwrap();
        fs::create_dir_all(root.join("web/node_modules/react/lib")).unwrap();
        fs::create_dir_all(root.join("svc/target/debug/deps")).unwrap();
        fs::create_dir_all(root.join("svc/src")).unwrap();

        let dirs = select_per_dir_watch_dirs(root, &None, &None);
        let names = rel(&dirs, root);

        for expected in ["web", "web/src", "web/src/components", "svc", "svc/src"] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected}: {names:?}"
            );
        }
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "nested node_modules must be pruned: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("target")),
            "nested target must be pruned: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            ".git is watched separately: {names:?}"
        );
    }

    /// Shallow-first ordering means a watch budget sheds the deepest dirs.
    #[test]
    fn select_per_dir_orders_shallow_first() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        fs::create_dir_all(root.join("z")).unwrap();

        let dirs = select_per_dir_watch_dirs(root, &None, &None);
        let depths: Vec<usize> = dirs.iter().map(|d| d.components().count()).collect();
        let mut sorted = depths.clone();
        sorted.sort_unstable();
        assert_eq!(depths, sorted, "must be shallow-first: {dirs:?}");
    }

    /// Custom glob pruning applies at every depth, like the top-level
    /// selector's semantics.
    #[test]
    fn select_per_dir_applies_custom_globs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("keep/skipme/deep")).unwrap();
        fs::create_dir_all(root.join("keep/sub")).unwrap();

        let (ignore, include) = build_globsets(&["skipme".to_string()]);
        let dirs = select_per_dir_watch_dirs(root, &ignore, &include);
        let names = rel(&dirs, root);

        assert!(names.contains(&"keep".to_string()));
        assert!(names.contains(&"keep/sub".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("skipme")),
            "custom-ignored subtree must be pruned: {names:?}"
        );
    }

    /// Symlinked dirs are never watched (or followed), so watches can't
    /// leave the workspace.
    #[cfg(unix)]
    #[test]
    fn select_per_dir_skips_symlinked_dirs() {
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();

        let dirs = select_per_dir_watch_dirs(root, &None, &None);
        let names = rel(&dirs, root);
        assert!(names.contains(&"real".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("linked")),
            "symlinked dir must not be watched: {names:?}"
        );
    }

    /// `.git` gets surgical watches: non-recursive `.git` + `refs`,
    /// recursive `refs/heads` + `refs/tags` — never `objects/`/`modules/`.
    #[test]
    fn per_dir_git_watches_are_surgical() {
        let temp = TempDir::new().unwrap();
        let gd = temp.path().join(".git");
        for d in [
            "objects/ab",
            "modules/sub/objects/cd",
            "refs/heads/feature",
            "refs/tags",
            "refs/remotes/origin",
        ] {
            fs::create_dir_all(gd.join(d)).unwrap();
        }

        let watches = per_dir_git_watches(&gd);
        let paths: Vec<(String, RecursiveMode)> = watches
            .iter()
            .map(|(p, m)| {
                (
                    p.strip_prefix(temp.path())
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    *m,
                )
            })
            .collect();

        assert!(paths.contains(&(".git".into(), RecursiveMode::NonRecursive)));
        assert!(paths.contains(&(".git/refs".into(), RecursiveMode::NonRecursive)));
        assert!(paths.contains(&(".git/refs/heads".into(), RecursiveMode::Recursive)));
        assert!(paths.contains(&(".git/refs/tags".into(), RecursiveMode::Recursive)));
        assert!(
            !paths.iter().any(|(p, _)| p.contains("objects")
                || p.contains("modules")
                || p.contains("remotes")),
            "objects/modules/remotes must never be watched: {paths:?}"
        );
    }

    /// Worktree git dirs (no `refs/`) degrade to just the non-recursive
    /// dir watch covering their `HEAD`/`index`.
    #[test]
    fn per_dir_git_watches_worktree_gitdir() {
        let temp = TempDir::new().unwrap();
        let gd = temp.path().join(".git/worktrees/wt");
        fs::create_dir_all(&gd).unwrap();

        let watches = per_dir_git_watches(&gd);
        assert_eq!(
            watches,
            vec![(gd.clone(), RecursiveMode::NonRecursive)],
            "worktree gitdir has no refs/: {watches:?}"
        );
    }

    #[test]
    fn scan_updates_classifies_created_dirs_and_files() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("newdir");
        let file = temp.path().join("newfile");
        fs::create_dir(&dir).unwrap();
        fs::write(&file, "x").unwrap();

        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Created,
            &[dir.clone(), file.clone()],
            &mut pruned,
            &mut added,
        );
        assert_eq!(added, vec![dir.clone()], "only dirs become subtree adds");
        // Structural event on an existing dir also prunes (re-arm for the
        // delete+recreate-within-one-debounce case); the file prune
        // candidate is rejected O(1) by the watcher thread.
        assert_eq!(pruned, vec![dir, file]);
    }

    #[test]
    fn scan_updates_prunes_a_directory_that_became_another_workspace() {
        let temp = TempDir::new().unwrap();
        let cloned = temp.path().join("cloned");
        fs::create_dir_all(cloned.join(".git")).unwrap();

        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Created,
            &[cloned.join(".git")],
            &mut pruned,
            &mut added,
        );

        assert!(
            pruned.contains(&cloned),
            "the marker's parent joins the prune list: {pruned:?}"
        );
    }

    #[test]
    fn scan_updates_prunes_removed_paths() {
        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Removed,
            &[PathBuf::from("/gone/dir")],
            &mut pruned,
            &mut added,
        );
        assert_eq!(pruned, vec![PathBuf::from("/gone/dir")]);
        assert!(added.is_empty());
    }

    /// FSEvents can coalesce a subtree removal into `Modified` on the
    /// vanished parent — state, not kind, must drive the prune.
    #[test]
    fn scan_updates_prunes_vanished_dir_on_modified() {
        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Modified,
            &[PathBuf::from("/vanished/pkg")],
            &mut pruned,
            &mut added,
        );
        assert_eq!(pruned, vec![PathBuf::from("/vanished/pkg")]);
        assert!(added.is_empty());
    }

    /// A `Modified` on an existing dir (metadata touch) is an add
    /// candidate only — no re-arm prune, so the watcher thread's
    /// `contains` check makes it a no-op.
    #[test]
    fn scan_updates_modified_existing_dir_is_add_candidate_only() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("d");
        fs::create_dir(&dir).unwrap();

        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Modified,
            std::slice::from_ref(&dir),
            &mut pruned,
            &mut added,
        );
        assert_eq!(added, vec![dir]);
        assert!(pruned.is_empty(), "no re-arm for non-structural events");
    }

    /// Rename shapes (`From`/`To`/`Both`) classify by on-disk state: the
    /// vanished old name prunes, the existing new name adds (with re-arm).
    #[test]
    fn scan_updates_classifies_renames_by_disk_state() {
        let temp = TempDir::new().unwrap();
        let new_dir = temp.path().join("new");
        fs::create_dir(&new_dir).unwrap();
        let old_dir = temp.path().join("old"); // never created — "moved away"

        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Renamed,
            &[old_dir.clone(), new_dir.clone()],
            &mut pruned,
            &mut added,
        );
        assert_eq!(pruned, vec![old_dir, new_dir.clone()]);
        assert_eq!(added, vec![new_dir]);
    }

    /// Symlinked dir created at runtime must not become a subtree add.
    #[cfg(unix)]
    #[test]
    fn scan_updates_skips_symlinked_dirs() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("t");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut pruned = Vec::new();
        let mut added = Vec::new();
        scan_per_dir_updates(
            FsEventKind::Created,
            std::slice::from_ref(&link),
            &mut pruned,
            &mut added,
        );
        assert!(added.is_empty(), "symlink must not be added: {added:?}");
    }
}

mod helper_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn is_top_level_child_distinguishes_levels() {
        let root = Path::new("/work/repo");
        assert!(is_top_level_child(Path::new("/work/repo/src"), root));
        assert!(is_top_level_child(Path::new("/work/repo/file.rs"), root));
        // Grandchildren, the root itself, and outside paths are not.
        assert!(!is_top_level_child(
            Path::new("/work/repo/src/main.rs"),
            root
        ));
        assert!(!is_top_level_child(root, root));
        assert!(!is_top_level_child(Path::new("/work/other"), root));
    }

    #[test]
    fn event_triggers_reconcile_only_for_top_level_structural() {
        let root = Path::new("/r");
        let child = [PathBuf::from("/r/pkg")];
        let nested = [PathBuf::from("/r/pkg/sub")];

        // Structural change to a direct child → reconcile.
        for kind in [
            FsEventKind::Created,
            FsEventKind::Removed,
            FsEventKind::Renamed,
        ] {
            assert!(event_triggers_reconcile(kind, &child, root), "{kind:?}");
        }
        // A bare modify is not structural.
        assert!(!event_triggers_reconcile(
            FsEventKind::Modified,
            &child,
            root
        ));
        // Structural but deeper than a direct child.
        assert!(!event_triggers_reconcile(
            FsEventKind::Created,
            &nested,
            root
        ));
        // Any top-level child in the batch is enough (drives the
        // one-reconcile-per-batch coalescing in the callback).
        let mixed = [PathBuf::from("/r/pkg/sub"), PathBuf::from("/r/newpkg")];
        assert!(event_triggers_reconcile(FsEventKind::Created, &mixed, root));
    }

    #[test]
    fn find_git_dir_discovers_real_repo_and_from_subdir() {
        // A real repo's `.git` is found from the root and from a subdir.
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();

        let gd = find_git_dir(temp.path()).expect("repo .git found");
        assert!(gd.ends_with(".git") && gd.is_dir(), "got {gd:?}");

        let sub = temp.path().join("crates/inner");
        fs::create_dir_all(&sub).unwrap();
        let gd_sub = find_git_dir(&sub).expect(".git found from subdir");
        assert!(
            gd_sub.ends_with(".git") && gd_sub.is_dir(),
            "got {gd_sub:?}"
        );
    }

    #[test]
    fn find_git_dir_none_when_no_repo() {
        // Hermetic: we create no `.git`, so `find_git_dir` must not return one
        // inside our tree (an ancestor repo's `.git`, outside it, is fine).
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let deep = root.join("no/git/here");
        fs::create_dir_all(&deep).unwrap();
        let result = find_git_dir(&deep);
        assert!(
            result.as_ref().is_none_or(|p| !p.starts_with(&root)),
            "must not find a .git inside a tree that contains none, got {result:?}"
        );
    }

    #[test]
    fn find_git_dir_rejects_bogus_gitlink() {
        // A planted `.git` file pointing at a non-git dir must NOT be
        // resolved/watched — git validation rejects it.
        let external = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        fs::write(
            proj.path().join(".git"),
            format!("gitdir: {}\n", external.path().display()),
        )
        .unwrap();

        let resolved = find_git_dir(proj.path());
        let external_canon = dunce::canonicalize(external.path()).unwrap();
        assert!(
            resolved.as_deref() != Some(external_canon.as_path()),
            "bogus gitlink target must not be watched, got {resolved:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_git_dir_rejects_symlinked_git_to_external_dir() {
        // A `.git` SYMLINK to an external (non-git) dir must NOT be followed
        // and watched: the cheap dir branch is gated on a real (non-symlink)
        // dir, and git validation rejects the target.
        let external = TempDir::new().unwrap(); // stands in for ~/.ssh, /etc
        let proj = TempDir::new().unwrap();
        std::os::unix::fs::symlink(external.path(), proj.path().join(".git")).unwrap();

        let resolved = find_git_dir(proj.path());
        let external_canon = dunce::canonicalize(external.path()).unwrap();
        assert!(
            resolved.as_deref() != Some(external_canon.as_path()),
            "symlinked .git to an external dir must not be watched, got {resolved:?}"
        );
    }

    #[test]
    fn find_git_dir_resolves_legitimate_gitlink() {
        // A `.git` FILE pointing at a real git dir (the worktree / submodule
        // layout) must resolve to that gitdir.
        let temp = TempDir::new().unwrap();
        let main = temp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let real_gitdir = git2::Repository::init(&main).unwrap().path().to_path_buf();
        let real_gitdir = dunce::canonicalize(&real_gitdir).unwrap_or(real_gitdir);

        let linked = temp.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", real_gitdir.display()),
        )
        .unwrap();

        let resolved = find_git_dir(&linked).expect("legit gitlink must resolve");
        assert_eq!(
            resolved, real_gitdir,
            "gitlink must resolve to the real gitdir"
        );
    }

    #[test]
    fn find_sl_dir_discovers_real_repo_and_from_subdir() {
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir(root.join(".sl")).unwrap();

        let sd = find_sl_dir(&root).expect("repo .sl found");
        assert!(
            sd.file_name().is_some_and(|n| n == ".sl") && sd.is_dir(),
            "got {sd:?}"
        );

        let sub = root.join("crates/inner");
        fs::create_dir_all(&sub).unwrap();
        let sd_sub = find_sl_dir(&sub).expect(".sl found from subdir");
        assert_eq!(sd_sub, sd, "subdir walk must find the ancestor .sl");
    }

    #[test]
    fn find_sl_dir_none_when_no_repo() {
        // Hermetic: no `.sl` created, so none must be found inside our tree.
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let deep = root.join("no/sl/here");
        fs::create_dir_all(&deep).unwrap();
        assert!(
            find_sl_dir(&deep).is_none_or(|p| !p.starts_with(&root)),
            "must not find a .sl in a tree that contains none"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_sl_dir_rejects_symlinked_sl_to_external_dir() {
        // A `.sl` SYMLINK to an external dir must not be followed/watched:
        // the dir branch is gated on a real (non-symlink) dir.
        let external = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        std::os::unix::fs::symlink(external.path(), proj.path().join(".sl")).unwrap();

        let resolved = find_sl_dir(proj.path());
        let external_canon = dunce::canonicalize(external.path()).unwrap();
        assert!(
            resolved.as_deref() != Some(external_canon.as_path()),
            "symlinked .sl to an external dir must not be watched, got {resolved:?}"
        );
    }

    #[test]
    fn should_watch_separate_vcs_dir_cases() {
        let watch = Path::new("/repo/crates/codegen");
        let internal = Path::new("/repo/crates/codegen/.sl");
        let external = Path::new("/repo/.sl");
        // Fan-out: the root is non-recursive, so always watch separately.
        assert!(should_watch_separate_vcs_dir(true, internal, watch));
        assert!(should_watch_separate_vcs_dir(true, external, watch));
        // Recursive root: an internal dir is already covered — must NOT be
        // re-watched (the double-watch the design warns against)...
        assert!(!should_watch_separate_vcs_dir(false, internal, watch));
        // ...but an external ancestor (subdir cwd) must still be watched, or
        // suppression silently breaks.
        assert!(should_watch_separate_vcs_dir(false, external, watch));
    }

    #[test]
    fn external_ancestor_sl_arms_in_recursive_root_mode() {
        // Subdir cwd whose `.sl` lives in an ancestor *outside* watch_path
        // (e.g. `grok` run in `crates/codegen`): the production guard must
        // still attach the watch under a recursive root (fanout=false).
        let temp = TempDir::new().unwrap();
        let repo = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir(repo.join(".sl")).unwrap();
        let watch_path = repo.join("crates/codegen");
        fs::create_dir_all(&watch_path).unwrap();

        let sd = find_sl_dir(&watch_path).expect("ancestor .sl discovered");
        assert!(should_watch_separate_vcs_dir(false, &sd, &watch_path));
    }

    #[test]
    #[serial_test::serial]
    fn sapling_enabled_respects_kill_switch() {
        // Clear the env var even if an assert panics mid-test.
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                // Safety: serialized test; no concurrent env access.
                unsafe { std::env::remove_var("GROK_FSNOTIFY_SAPLING") };
            }
        }
        let _restore = Restore;

        unsafe { std::env::remove_var("GROK_FSNOTIFY_SAPLING") };
        assert!(sapling_enabled(), "default (unset) is enabled");
        for off in ["0", "false"] {
            unsafe { std::env::set_var("GROK_FSNOTIFY_SAPLING", off) };
            assert!(!sapling_enabled(), "{off:?} must disable Sapling");
        }
        unsafe { std::env::set_var("GROK_FSNOTIFY_SAPLING", "1") };
        assert!(sapling_enabled(), "any other value stays enabled");
    }

    #[test]
    fn select_capped_boundary_is_inclusive() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        for i in 0..3 {
            fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
        }
        // Exactly `cap` non-ignored dirs must fan out (inclusive `<=`); this
        // pins the edge so flipping the comparison to `<` would fail.
        assert_eq!(
            select_top_level_watch_dirs_capped(root, &None, &None, 3).map(|v| v.len()),
            Some(3),
            "count == cap must fan out"
        );
        // One past the cap falls back.
        assert!(
            select_top_level_watch_dirs_capped(root, &None, &None, 2).is_none(),
            "count > cap must fall back"
        );
        // And comfortably within the cap.
        assert_eq!(
            select_top_level_watch_dirs_capped(root, &None, &None, 4).map(|v| v.len()),
            Some(3)
        );
    }

    #[test]
    fn select_capped_does_not_count_ignored_toward_cap() {
        // Gitignored top-level dirs must not push a repo over the cap.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "ignored_*/\n").unwrap();
        fs::create_dir_all(root.join("real_0")).unwrap();
        fs::create_dir_all(root.join("real_1")).unwrap();
        for i in 0..5 {
            fs::create_dir_all(root.join(format!("ignored_{i}"))).unwrap();
        }
        // 7 total dirs, 5 ignored: with a cap of 2 the 2 non-ignored fit.
        let result = select_top_level_watch_dirs_capped(root, &None, &None, 2);
        assert_eq!(
            result.map(|v| v.len()),
            Some(2),
            "ignored dirs must not count toward the fan-out cap"
        );
    }

    #[test]
    fn diff_watches_add_remove_noop_combined() {
        let p = |s: &str| PathBuf::from(s);
        let set = |items: &[&str]| items.iter().map(|s| p(s)).collect::<HashSet<_>>();
        let sorted = |mut v: Vec<PathBuf>| {
            v.sort();
            v
        };

        // Add only.
        let (add, rem) = diff_watches(&set(&["/a", "/b"]), &set(&[]));
        assert_eq!(sorted(add), vec![p("/a"), p("/b")]);
        assert!(rem.is_empty());

        // Remove only.
        let (add, rem) = diff_watches(&set(&[]), &set(&["/a"]));
        assert!(add.is_empty());
        assert_eq!(rem, vec![p("/a")]);

        // No-op.
        let (add, rem) = diff_watches(&set(&["/a"]), &set(&["/a"]));
        assert!(add.is_empty() && rem.is_empty());

        // Combined.
        let (add, rem) = diff_watches(&set(&["/a", "/b"]), &set(&["/b", "/c"]));
        assert_eq!(sorted(add), vec![p("/a")]);
        assert_eq!(rem, vec![p("/c")]);
    }
}
