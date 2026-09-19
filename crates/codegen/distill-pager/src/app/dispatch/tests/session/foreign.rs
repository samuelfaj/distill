// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use distill_shell::session::unified_list::ListScope;

use crate::views::modal::ActiveModal;
use crate::views::session_picker::{PickerItem, SourceFilter, build_entry_map};
use distill_foreign_sessions::ForeignSessionTool;

fn make_foreign_entry(
    id: &str,
    source: &str,
    cwd: &str,
) -> crate::app::app_view::SessionPickerEntry {
    let mut entry = make_picker_entry(id, cwd);
    entry.source = source.into();
    entry
}

fn at(
    mut entry: crate::app::app_view::SessionPickerEntry,
    seconds: i64,
) -> crate::app::app_view::SessionPickerEntry {
    let timestamp = chrono::DateTime::from_timestamp(seconds, 0).unwrap();
    entry.updated_at = timestamp;
    entry.last_active_at = Some(timestamp);
    entry
}

fn modal_entries(app: &AppView) -> &[crate::app::app_view::SessionPickerEntry] {
    let Some(ActiveModal::SessionPicker {
        entries: Some(entries),
        ..
    }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    entries
}

#[test]
fn foreign_result_interleaves_deduplicates_and_empty_clears_only_external() {
    let mut app = test_app();
    app.foreign_session_scan_seq = 4;
    app.session_picker_lanes.foreign_loading = true;
    app.session_picker_entries = Some(vec![at(make_picker_entry("native", "/repo"), 20)]);

    let effects = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![
                at(make_foreign_entry("old", "claude", "/repo"), 10),
                at(make_foreign_entry("new", "codex", "/repo"), 30),
                at(make_foreign_entry("old", "claude", "/repo"), 10),
            ],
            seq: 4,
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    let ids: Vec<_> = app
        .session_picker_entries
        .as_ref()
        .unwrap()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(ids, ["new", "native", "old"]);

    app.session_picker_lanes.foreign_loading = true;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![],
            seq: 4,
        }),
        &mut app,
    );
    let entries = app.session_picker_entries.as_ref().unwrap();
    assert_eq!(entries.len(), 1);
    let Some(entry) = entries.first() else {
        panic!("expected a picker entry: {entries:?}");
    };
    assert_eq!(entry.id, "native");
}

#[test]
fn foreign_generation_drops_stale_closed_and_pre_reopen_results() {
    let mut app = test_app();
    app.foreign_session_scan_seq = 7;
    app.session_picker_entries = Some(vec![make_picker_entry("native", "/repo")]);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("stale", "cursor", "/repo")],
            seq: 6,
        }),
        &mut app,
    );
    assert_eq!(app.session_picker_entries.as_ref().unwrap().len(), 1);

    app.session_picker_entries = None;
    let _ = dispatch(Action::SessionPickerClosed, &mut app);
    let closed_seq = app.foreign_session_scan_seq;
    let _ = dispatch(Action::FetchSessionList, &mut app);
    let reopened_seq = app.foreign_session_scan_seq;
    app.session_picker_lanes.foreign_loading = true;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("closed", "claude", "/repo")],
            seq: closed_seq,
        }),
        &mut app,
    );
    assert!(app.session_picker_entries.is_none());

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("reopened", "claude", "/repo")],
            seq: reopened_seq,
        }),
        &mut app,
    );
    assert_eq!(
        app.session_picker_entries
            .as_ref()
            .and_then(|e| e.first())
            .map(|e| e.id.as_str()),
        Some("reopened")
    );
}

#[test]
fn modal_refetch_clears_orphaned_welcome_foreign_loading() {
    let mut app = test_app_with_agent();
    app.foreign_session_compat = distill_foreign_sessions::EnabledForeignSessionSources {
        claude: true,
        codex: true,
        cursor: true,
    };
    app.session_picker_lanes.foreign_loading = true;
    open_session_picker_with(&mut app, vec![]);

    let effects = dispatch(Action::FetchSessionList, &mut app);

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ScanForeignSessions { .. }))
    );
    assert!(!app.session_picker_lanes.foreign_loading);
    let Some(ActiveModal::SessionPicker { lanes, .. }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    assert!(lanes.foreign_loading);
}

#[test]
fn modal_foreign_scan_uses_native_list_cwd() {
    let mut app = test_app_with_agent();
    app.cwd = PathBuf::from("/native-list-cwd");
    app.agents.get_mut(&AgentId(0)).unwrap().session.cwd = PathBuf::from("/agent-worktree-cwd");
    app.foreign_session_compat = distill_foreign_sessions::EnabledForeignSessionSources {
        claude: true,
        codex: true,
        cursor: true,
    };
    open_session_picker_with(&mut app, vec![]);

    let effects = dispatch(Action::FetchSessionList, &mut app);

    let [
        Effect::FetchSessionList { .. },
        Effect::ScanForeignSessions { cwd, .. },
    ] = effects.as_slice()
    else {
        panic!("expected native and foreign picker effects");
    };
    assert_eq!(cwd, &app.cwd);
}

#[test]
fn modal_without_foreign_lane_does_not_consume_welcome_result() {
    let mut app = test_app_with_agent();
    app.foreign_session_scan_seq = 12;
    app.session_picker_entries = Some(vec![make_picker_entry("welcome-native", "/repo")]);
    app.session_picker_lanes.foreign_loading = true;
    open_session_picker_with(&mut app, vec![make_picker_entry("modal-native", "/repo")]);

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("welcome-foreign", "cursor", "/repo")],
            seq: 12,
        }),
        &mut app,
    );

    assert_eq!(
        modal_entries(&app).first().map(|e| e.id.as_str()),
        Some("modal-native")
    );
    assert!(
        app.session_picker_entries
            .as_ref()
            .unwrap()
            .iter()
            .any(|entry| entry.id == "welcome-foreign")
    );
    assert!(!app.session_picker_lanes.foreign_loading);
}

#[test]
fn native_empty_waits_for_foreign_and_foreign_only_rows_survive() {
    let mut app = test_app();
    app.session_picker_list_seq = 1;
    app.foreign_session_scan_seq = 2;
    app.session_picker_loading = true;
    app.session_picker_lanes.foreign_loading = true;

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq: 1,
            query: None,
        }),
        &mut app,
    );
    assert!(!app.session_picker_loading);
    assert!(app.session_picker_lanes.foreign_loading);
    assert!(app.session_picker_entries.is_none());
    assert!(app.session_picker_lanes.pending_notice.is_some());

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("foreign-only", "cursor", "/repo")],
            seq: 2,
        }),
        &mut app,
    );
    assert!(!app.session_picker_lanes.foreign_loading);
    assert!(app.session_picker_lanes.pending_notice.is_none());
    assert_eq!(
        app.session_picker_entries
            .as_ref()
            .and_then(|e| e.first())
            .map(|e| e.id.as_str()),
        Some("foreign-only")
    );
}

#[test]
fn foreign_empty_then_native_empty_finishes_once_without_resurrecting() {
    let mut app = test_app();
    app.session_picker_list_seq = 3;
    app.foreign_session_scan_seq = 5;
    app.session_picker_loading = true;
    app.session_picker_lanes.foreign_loading = true;

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![],
            seq: 5,
        }),
        &mut app,
    );
    assert!(!app.session_picker_lanes.foreign_loading);
    assert!(app.session_picker_loading);

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq: 3,
            query: None,
        }),
        &mut app,
    );
    assert!(!app.session_picker_loading);
    assert!(app.session_picker_entries.is_none());
    assert!(app.session_picker_lanes.pending_notice.is_none());
}

#[test]
fn modal_native_failure_waits_for_foreign_rows_before_toast() {
    let mut app = test_app_with_agent();
    app.session_picker_list_seq = 6;
    app.foreign_session_scan_seq = 7;
    open_session_picker_with(&mut app, vec![]);
    if let Some(ActiveModal::SessionPicker { loading, lanes, .. }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        *loading = true;
        lanes.foreign_loading = true;
    }

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            error: "native failed".into(),
            seq: 6,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.toast.is_none())
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("foreign-only", "codex", "/repo")],
            seq: 7,
        }),
        &mut app,
    );
    assert_eq!(
        modal_entries(&app).first().map(|e| e.id.as_str()),
        Some("foreign-only")
    );
    assert!(read_toast(&app).contains("native failed"));
}

#[test]
fn modal_empty_notice_waits_until_both_lanes_are_empty() {
    let mut app = test_app_with_agent();
    app.session_picker_list_seq = 9;
    app.foreign_session_scan_seq = 10;
    open_session_picker_with(&mut app, vec![]);
    if let Some(ActiveModal::SessionPicker { loading, lanes, .. }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        *loading = true;
        lanes.foreign_loading = true;
    }
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq: 9,
            query: None,
        }),
        &mut app,
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.toast.is_none())
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![],
            seq: 10,
        }),
        &mut app,
    );
    assert!(read_toast(&app).contains("No sessions found"));
}

#[test]
fn welcome_selection_survives_foreign_insertion_with_viewport_offset() {
    let mut app = test_app();
    app.session_picker_grouped = false;
    app.foreign_session_scan_seq = 8;
    app.session_picker_lanes.foreign_loading = true;
    // Pin All: the insertion only shifts rows when foreign entries are visible.
    app.session_picker_source_filter = SourceFilter::All;
    app.session_picker_entries = Some(vec![
        at(make_picker_entry("a", "/repo"), 20),
        at(make_picker_entry("b", "/repo"), 10),
    ]);
    app.session_picker_state.selected = 1;
    app.session_picker_state.scroll_offset = Some(1);

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![at(make_foreign_entry("new", "codex", "/repo"), 30)],
            seq: 8,
        }),
        &mut app,
    );

    assert_eq!(app.session_picker_state.selected, 2);
    assert_eq!(app.session_picker_state.scroll_offset, Some(2));
    let Some(selected) = app.session_picker_entries.as_ref().and_then(|e| e.get(2)) else {
        panic!("expected picker entry at index 2");
    };
    assert_eq!(
        (selected.source.as_str(), selected.id.as_str()),
        ("local", "b")
    );
}

#[test]
fn modal_selection_survives_native_and_foreign_completion_races() {
    let mut app = test_app_with_agent();
    app.session_picker_list_seq = 2;
    app.foreign_session_scan_seq = 3;
    open_session_picker_with(
        &mut app,
        vec![
            at(make_picker_entry("a", "/repo"), 20),
            at(make_picker_entry("b", "/repo"), 10),
        ],
    );
    if let Some(ActiveModal::SessionPicker {
        state,
        lanes,
        source_filter,
        ..
    }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        state.selected = 2;
        state.scroll_offset = Some(1);
        lanes.foreign_loading = true;
        // Pin All: the insertion only shifts rows when foreign entries are visible.
        *source_filter = SourceFilter::All;
    }

    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![at(make_foreign_entry("new", "cursor", "/repo"), 30)],
            seq: 3,
        }),
        &mut app,
    );
    let Some(ActiveModal::SessionPicker { state, .. }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    assert_eq!(state.selected, 3);
    assert_eq!(state.scroll_offset, Some(2));
    let map = build_entry_map(
        Some(modal_entries(&app)),
        None,
        "",
        true,
        false,
        SourceFilter::All,
        Some("repo"),
    );
    let Some(PickerItem::Fuzzy { original_index }) =
        map.get(state.selected).and_then(|item| item.as_ref())
    else {
        panic!("selection must remain on a row");
    };
    let Some(entry) = modal_entries(&app).get(*original_index) else {
        panic!("missing picker row {original_index}");
    };
    assert_eq!(entry.id, "b");

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            scope: ListScope::Cwd,
            sessions: vec![
                at(make_picker_entry("a", "/repo"), 20),
                at(make_picker_entry("b", "/repo"), 10),
            ],
            partial: None,
            seq: 2,
            query: None,
        }),
        &mut app,
    );
    let Some(ActiveModal::SessionPicker { state, .. }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    let map = build_entry_map(
        Some(modal_entries(&app)),
        None,
        "",
        true,
        false,
        SourceFilter::All,
        Some("repo"),
    );
    let Some(PickerItem::Fuzzy { original_index }) =
        map.get(state.selected).and_then(|item| item.as_ref())
    else {
        panic!("selection must remain on a row");
    };
    let Some(entry) = modal_entries(&app).get(*original_index) else {
        panic!("missing picker row {original_index}");
    };
    assert_eq!(entry.id, "b");
}

#[test]
fn external_filter_clears_and_suppresses_native_content_state() {
    let mut app = test_app();
    app.session_picker_grouped = false;
    app.session_picker_entries = Some(vec![
        make_picker_entry("native", "/repo"),
        make_foreign_entry("foreign", "codex", "/repo"),
    ]);
    app.session_picker_content_results = Some(vec![content_hit("native-hit")]);
    app.session_picker_content_loading = true;
    app.session_picker_state.expanded.insert(0);
    app.session_picker_source_filter = SourceFilter::Headless;
    let old_detail_seq = app.session_picker_detail_seq;

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);

    assert!(matches!(
        effects.as_slice(),
        [Effect::FetchSessionList { .. }]
    ));
    assert_eq!(app.session_picker_source_filter, SourceFilter::External);
    assert!(app.session_picker_content_results.is_none());
    assert!(!app.session_picker_content_loading);
    assert!(app.session_picker_state.expanded.is_empty());
    assert!(app.session_picker_detail_seq > old_detail_seq);
    assert!(dispatch(Action::TriggerDeepSearch, &mut app).is_empty());
    assert!(
        dispatch(
            Action::ExpandSessionCard {
                source: "local".into(),
                session_id: "native".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Action::DeleteSession {
                source: "local".into(),
                session_id: "native".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Action::PickContentSession {
                session_id: "native-hit".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Action::PickContentSessionInWorktree {
                session_id: "native-hit".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .is_empty()
    );

    let map = build_entry_map(
        app.session_picker_entries.as_deref(),
        Some(&[content_hit("native-hit")]),
        "",
        false,
        true,
        SourceFilter::External,
        None,
    );
    assert_eq!(map.len(), 1);
    let Some(PickerItem::Fuzzy { original_index }) = map.first().and_then(|item| item.as_ref())
    else {
        panic!("external row missing");
    };
    assert_eq!(
        app.session_picker_entries
            .as_ref()
            .and_then(|e| e.get(*original_index))
            .map(|e| e.id.as_str()),
        Some("foreign")
    );
}

#[test]
fn modal_external_filter_clears_native_content_and_blocks_forced_search() {
    let mut app = test_app_with_agent();
    open_session_picker_with(
        &mut app,
        vec![
            make_picker_entry("native", "/repo"),
            make_foreign_entry("foreign", "claude", "/repo"),
        ],
    );
    if let Some(ActiveModal::SessionPicker {
        state,
        content_results,
        content_loading,
        source_filter,
        ..
    }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        *source_filter = SourceFilter::Headless;
        *content_results = Some(vec![content_hit("native-hit")]);
        *content_loading = true;
        state.set_query("native");
        state.expanded.insert(0);
    }

    let _ = dispatch(Action::CycleSessionSourceFilter, &mut app);

    let Some(ActiveModal::SessionPicker {
        state,
        content_results,
        content_loading,
        source_filter,
        ..
    }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    assert_eq!(*source_filter, SourceFilter::External);
    assert!(content_results.is_none());
    assert!(!*content_loading);
    assert!(state.expanded.is_empty());
    assert!(dispatch(Action::ForceDeepSearch, &mut app).is_empty());
}

#[test]
fn cycle_reaches_every_filter_with_foreign_present() {
    let mut app = test_app();
    app.session_picker_entries = Some(vec![
        make_picker_entry("native", "/repo"),
        make_foreign_entry("foreign", "claude", "/repo"),
    ]);
    for expected in [
        SourceFilter::Headless,
        SourceFilter::External,
        SourceFilter::All,
        SourceFilter::Local,
        SourceFilter::Remote,
        SourceFilter::Grok,
    ] {
        let _ = dispatch(Action::CycleSessionSourceFilter, &mut app);
        assert_eq!(app.session_picker_source_filter, expected);
    }
}

#[test]
fn empty_headless_welcome_keeps_picker_open() {
    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_picker_entry("native", "/repo")]);
    app.session_picker_loading = false;

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);
    let seq = match effects.as_slice() {
        [Effect::FetchSessionList { seq, .. }] => *seq,
        other => panic!("expected FetchSessionList, got {other:?}"),
    };

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            scope: ListScope::Cwd,
            sessions: vec![],
            partial: None,
            seq,
            query: None,
        }),
        &mut app,
    );

    assert!(
        app.session_picker_entries
            .as_ref()
            .is_some_and(|entries| entries.is_empty()),
        "empty Headless must keep Some([]) so show_picker stays true"
    );
    assert!(!app.session_picker_loading);
    assert!(
        app.welcome_toast.is_none(),
        "a filtered-empty page is not 'no sessions in this directory'"
    );
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::External);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchSessionList { .. })),
        "leaving Headless must still refetch under Exclude"
    );
}

#[test]
fn failed_headless_welcome_keeps_picker_open() {
    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_picker_entry("native", "/repo")]);
    app.session_picker_loading = false;

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);
    let seq = match effects.as_slice() {
        [Effect::FetchSessionList { seq, .. }] => *seq,
        other => panic!("expected FetchSessionList, got {other:?}"),
    };

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SessionListFailed {
            host: SessionPickerHost::Welcome,
            generation: app.session_picker_generation,
            error: "boom".into(),
            seq,
            query: None,
        }),
        &mut app,
    );

    assert!(
        app.session_picker_entries
            .as_ref()
            .is_some_and(|entries| entries.is_empty()),
        "failed Headless must keep Some([]) so show_picker stays true"
    );
    assert!(!app.session_picker_loading);
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::External);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchSessionList { .. })),
        "leaving Headless must still refetch under Exclude"
    );
}

#[test]
fn cycle_refetches_when_crossing_headless() {
    use distill_shell::session::unified_list::HeadlessPolicy;

    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_picker_entry("native", "/repo")]);
    app.session_picker_loading = false;
    app.session_picker_pending_delete = Some(crate::views::session_picker::PendingDelete {
        source: "local".into(),
        session_id: "native".into(),
        cwd: "/repo".into(),
    });
    let seq_before = app.session_picker_list_seq;

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);
    match effects.as_slice() {
        [
            Effect::FetchSessionList {
                query: None,
                seq,
                headless_policy,
                ..
            },
        ] => {
            assert_eq!(*headless_policy, HeadlessPolicy::Only);
            assert_eq!(*seq, seq_before + 1, "stale in-flight lists must drop");
        }
        other => panic!("expected FetchSessionList, got {other:?}"),
    }
    assert!(
        app.session_picker_loading,
        "the page cannot be served from cache, so it is loading"
    );
    assert!(
        app.session_picker_pending_delete.is_none(),
        "welcome filter changes must disarm deletion"
    );

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::External);
    match effects.as_slice() {
        [
            Effect::FetchSessionList {
                headless_policy, ..
            },
        ] => assert_eq!(*headless_policy, HeadlessPolicy::Exclude),
        other => panic!("expected FetchSessionList, got {other:?}"),
    }

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::All);
    assert!(effects.is_empty());
}

#[test]
fn leaving_headless_drops_stale_natives_and_spins_on_all() {
    let mut app = test_app();
    let mut headless = make_picker_entry("h1", "/repo");
    headless.session_kind = Some("headless".into());
    app.session_picker_entries = Some(vec![headless, make_foreign_entry("f1", "claude", "/repo")]);
    app.session_picker_source_filter = SourceFilter::Headless;
    app.session_picker_loading = false;

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::External);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchSessionList { .. })),
        "leaving Headless must refetch under Exclude"
    );
    let entries = app
        .session_picker_entries
        .as_ref()
        .expect("picker stays up");
    assert!(
        entries
            .iter()
            .all(|entry| crate::app::is_foreign_picker_source(&entry.source)),
        "stale Headless natives must not remain after the policy change"
    );
    assert!(app.session_picker_loading);
    assert!(
        !crate::views::session_picker::loading_spinner_active(
            app.session_picker_entries.as_deref(),
            app.session_picker_source_filter,
            app.session_picker_loading,
            &app.session_picker_lanes,
        ),
        "External must keep showing cached foreign rows during the Exclude refetch"
    );

    let _ = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::All);
    assert!(
        crate::views::session_picker::loading_spinner_active(
            app.session_picker_entries.as_deref(),
            app.session_picker_source_filter,
            app.session_picker_loading,
            &app.session_picker_lanes,
        ),
        "All must spin while the Exclude refetch is in flight"
    );
}

#[test]
fn modal_cycle_refetches_when_entering_headless() {
    use distill_shell::session::unified_list::HeadlessPolicy;

    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_picker_entry("native", "/repo")]);

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    match effects.as_slice() {
        [
            Effect::FetchSessionList {
                headless_policy, ..
            },
        ] => assert_eq!(*headless_policy, HeadlessPolicy::Only),
        other => panic!("expected FetchSessionList, got {other:?}"),
    }
    let Some(ActiveModal::SessionPicker {
        source_filter,
        loading,
        ..
    }) = app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    else {
        panic!("modal picker missing");
    };
    assert_eq!(*source_filter, SourceFilter::Headless);
    assert!(*loading);
}

#[test]
fn headless_page_content_search_uses_only_policy() {
    use distill_shell::session::unified_list::HeadlessPolicy;

    let mut app = test_app();
    app.session_picker_entries = Some(vec![make_picker_entry("native", "/repo")]);
    app.session_picker_state.set_query("batch");
    app.session_picker_content_results = Some(vec![content_hit("grok-page-hit")]);

    let effects = dispatch(Action::CycleSessionSourceFilter, &mut app);
    assert_eq!(app.session_picker_source_filter, SourceFilter::Headless);
    match effects.as_slice() {
        [
            Effect::FetchSessionList { .. },
            Effect::DeepSearchSessions {
                query,
                headless_policy,
                ..
            },
        ] => {
            assert_eq!(query, "batch");
            assert_eq!(*headless_policy, HeadlessPolicy::Only);
        }
        other => panic!("expected refetch + search restart, got {other:?}"),
    }
    assert!(
        app.session_picker_content_loading,
        "the restarted search must show as in flight"
    );

    let effects = dispatch(Action::ForceDeepSearch, &mut app);
    match effects.as_slice() {
        [
            Effect::DeepSearchSessions {
                headless_policy, ..
            },
        ] => assert_eq!(*headless_policy, HeadlessPolicy::Only),
        other => panic!("expected DeepSearchSessions, got {other:?}"),
    }
}

#[test]
fn active_modal_owns_stale_and_external_deep_search_results() {
    for external in [false, true] {
        let mut app = test_app_with_agent();
        open_session_picker_with(&mut app, vec![make_picker_entry("modal", "/repo")]);
        app.session_picker_deep_search_seq = 7;
        if let Some(ActiveModal::SessionPicker {
            deep_search_seq,
            source_filter,
            ..
        }) = get_active_agent_mut(&mut app)
            .unwrap()
            .active_modal
            .as_mut()
        {
            *deep_search_seq = if external { 7 } else { 8 };
            if external {
                *source_filter = SourceFilter::External;
            }
        }

        let _ = dispatch(
            Action::TaskComplete(TaskResult::DeepSearchResults {
                host: SessionPickerHost::AgentModal,
                generation: modal_picker_generation(&app),
                results: vec![content_hit("must-not-reach-welcome")],
                seq: 7,
            }),
            &mut app,
        );

        assert!(app.session_picker_content_results.is_none());
        let Some(ActiveModal::SessionPicker {
            content_results, ..
        }) = app
            .agents
            .get(&AgentId(0))
            .and_then(|a| a.active_modal.as_ref())
        else {
            panic!("modal picker missing");
        };
        assert!(content_results.is_none());
    }
}

#[test]
fn detail_result_revalidates_source_id_and_detail_seq_after_reorder() {
    let mut app = test_app_with_agent();
    open_session_picker_with(
        &mut app,
        vec![
            at(make_picker_entry("target", "/repo"), 20),
            at(make_picker_entry("other", "/repo"), 10),
        ],
    );
    if let Some(ActiveModal::SessionPicker { lanes, .. }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        lanes.foreign_loading = true;
    }
    let effects = dispatch(
        Action::ExpandSessionCard {
            source: "local".into(),
            session_id: "target".into(),
        },
        &mut app,
    );
    let [
        Effect::LoadCardDetail {
            host: SessionPickerHost::AgentModal,
            source,
            session_id,
            seq,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("expected identity-addressed detail effect");
    };
    assert_eq!(source, "local");
    assert_eq!(session_id, "target");
    let stale_seq = *seq;

    app.foreign_session_scan_seq = 4;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![at(make_foreign_entry("new", "cursor", "/repo"), 30)],
            seq: 4,
        }),
        &mut app,
    );
    let detail = crate::app::app_view::CardDetail {
        turn_count: 7,
        tool_call_count: 3,
        first_prompt_preview: "first".into(),
    };
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            source: "local".into(),
            session_id: "target".into(),
            seq: stale_seq,
            detail: detail.clone(),
        }),
        &mut app,
    );
    assert!(
        modal_entries(&app)
            .iter()
            .all(|entry| entry.card_detail.is_none())
    );

    if let Some(ActiveModal::SessionPicker {
        entries: Some(entries),
        ..
    }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        entries
            .iter_mut()
            .find(|entry| entry.id == "target")
            .unwrap()
            .source = "cursor".into();
    }
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            source: "local".into(),
            session_id: "target".into(),
            seq: modal_picker_detail_seq(&app),
            detail: detail.clone(),
        }),
        &mut app,
    );
    assert!(
        modal_entries(&app)
            .iter()
            .all(|entry| entry.card_detail.is_none())
    );
    if let Some(ActiveModal::SessionPicker {
        entries: Some(entries),
        ..
    }) = get_active_agent_mut(&mut app)
        .unwrap()
        .active_modal
        .as_mut()
    {
        entries
            .iter_mut()
            .find(|entry| entry.id == "target")
            .unwrap()
            .source = "local".into();
    }

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CardDetailLoaded {
            host: SessionPickerHost::AgentModal,
            generation: modal_picker_generation(&app),
            source: "local".into(),
            session_id: "target".into(),
            seq: modal_picker_detail_seq(&app),
            detail,
        }),
        &mut app,
    );
    assert_eq!(
        modal_entries(&app)
            .iter()
            .find(|entry| entry.id == "target")
            .and_then(|entry| entry.card_detail.as_ref())
            .map(|detail| detail.turn_count),
        Some(7)
    );
}

#[test]
fn colliding_native_and_foreign_ids_use_source_at_initiation() {
    let mut app = test_app_with_agent();
    open_session_picker_with(
        &mut app,
        vec![
            make_foreign_entry("shared-id", "codex", "/repo"),
            make_picker_entry("shared-id", "/repo"),
        ],
    );

    let effects = dispatch(
        Action::ExpandSessionCard {
            source: "local".into(),
            session_id: "shared-id".into(),
        },
        &mut app,
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::LoadCardDetail {
            source,
            session_id,
            ..
        }] if source == "local" && session_id == "shared-id"
    ));
    assert!(
        dispatch(
            Action::ExpandSessionCard {
                source: "codex".into(),
                session_id: "shared-id".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Action::DeleteSession {
                source: "codex".into(),
                session_id: "shared-id".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(matches!(
        dispatch(
            Action::DeleteSession {
                source: "local".into(),
                session_id: "shared-id".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .as_slice(),
        [Effect::DeleteSession { session_id, .. }] if session_id == "shared-id"
    ));
}

#[test]
fn gated_foreign_pick_replaces_all_prior_startup_intents() {
    let mut app = test_app_with_agent();
    let old_id = AgentId(0);
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/repo"),
    };
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "must-not-load".into(),
            session_cwd: Some(PathBuf::from("/other")),
            chat_kind: true,
        });
    app.deferred_startup.worktree = true;
    app.deferred_startup.worktree_label = Some("stale".into());
    app.deferred_startup.worktree_ref = Some("stale-ref".into());
    app.deferred_startup.preferred_session_id = Some("stale-id".into());
    app.deferred_startup.new_session = true;
    app.deferred_startup.prompt = Some("stale prompt".into());
    app.deferred_startup.pending_chat = true;
    open_session_picker_with(
        &mut app,
        vec![make_foreign_entry("codex-deferred", "codex", "/repo")],
    );

    assert!(dispatch(Action::PickSession(0), &mut app).is_empty());
    assert!(matches!(
        app.deferred_startup.session.as_ref(),
        Some(crate::app::session_startup::DeferredSessionStartup::ForeignResume {
            tool: ForeignSessionTool::Codex,
            native_id,
        }) if native_id == "codex-deferred"
    ));
    assert!(!app.deferred_startup.worktree);
    assert!(app.deferred_startup.worktree_label.is_none());
    assert!(app.deferred_startup.worktree_ref.is_none());
    assert!(app.deferred_startup.preferred_session_id.is_none());
    assert!(!app.deferred_startup.new_session);
    assert!(app.deferred_startup.prompt.is_none());
    assert!(!app.deferred_startup.pending_chat);

    app.trust_state = TrustState::Done;
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    let effects = drain_startup_actions(&mut app);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::CreateSession { .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::CreateWorktreeSession { .. }))
    );
    assert!(
        app.agents
            .get(&old_id)
            .is_some_and(|a| a.session.pending_prompts.is_empty())
    );
    let new_id = AgentId(1);
    assert_eq!(app.active_view, ActiveView::Agent(new_id));
    assert_eq!(
        app.agents
            .get(&new_id)
            .and_then(|a| a.session.pending_prompts.front())
            .map(|prompt| prompt.text.as_str()),
        Some("/resume-codex codex-deferred")
    );
}

#[test]
fn welcome_and_modal_foreign_picks_always_target_fresh_sessions() {
    let mut welcome = test_app();
    welcome.session_picker_entries =
        Some(vec![make_foreign_entry("codex-native", "codex", "/repo")]);
    let effects = dispatch(Action::PickSession(0), &mut welcome);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::CreateSession { .. }))
    );
    assert_eq!(
        welcome
            .agents
            .get(&AgentId(0))
            .and_then(|a| a.session.pending_prompts.front())
            .map(|prompt| prompt.text.as_str()),
        Some("/resume-codex codex-native")
    );

    let mut modal = test_app_with_agent();
    open_session_picker_with(
        &mut modal,
        vec![make_foreign_entry("cursor-native", "cursor", "/repo")],
    );
    let effects = dispatch(Action::PickSession(0), &mut modal);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::CreateSession { .. }))
    );
    assert!(
        modal
            .agents
            .get(&AgentId(0))
            .is_some_and(|a| a.session.pending_prompts.is_empty())
    );
    assert_eq!(
        modal
            .agents
            .get(&AgentId(1))
            .and_then(|a| a.session.pending_prompts.front())
            .map(|prompt| prompt.text.as_str()),
        Some("/resume-cursor cursor-native")
    );
}

#[test]
fn foreign_selection_and_mutation_guards_remain_central() {
    let mut app = test_app_with_agent();
    open_session_picker_with(
        &mut app,
        vec![make_foreign_entry("foreign-id", "cursor", "/repo")],
    );
    assert!(dispatch(Action::PickSessionInWorktree(0), &mut app).is_empty());
    assert!(
        dispatch(
            Action::ExpandSessionCard {
                source: "cursor".into(),
                session_id: "foreign-id".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Action::DeleteSession {
                source: "cursor".into(),
                session_id: "foreign-id".into(),
                cwd: "/repo".into(),
            },
            &mut app,
        )
        .is_empty()
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.active_modal.is_some())
    );
}

#[test]
fn chat_picker_never_launches_or_accepts_foreign_scan() {
    let mut app = test_app();
    app.chat_mode = true;
    app.foreign_session_compat = distill_foreign_sessions::EnabledForeignSessionSources {
        claude: true,
        codex: true,
        cursor: true,
    };
    let effects = dispatch(Action::FetchSessionList, &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::FetchSessionList { .. }]
    ));
    app.session_picker_entries = Some(vec![make_conversation_entry("chat")]);
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![make_foreign_entry("foreign", "claude", "/repo")],
            seq: app.foreign_session_scan_seq,
        }),
        &mut app,
    );
    assert_eq!(app.session_picker_entries.as_ref().unwrap().len(), 1);
}

#[test]
fn native_fetch_effect_precedes_background_foreign_gate() {
    let mut app = test_app();
    app.foreign_session_compat = distill_foreign_sessions::EnabledForeignSessionSources {
        claude: true,
        codex: true,
        cursor: true,
    };

    let effects = dispatch(Action::FetchSessionList, &mut app);

    assert!(matches!(
        effects.as_slice(),
        [
            Effect::FetchSessionList { .. },
            Effect::ScanForeignSessions { .. }
        ]
    ));
    assert!(app.session_picker_lanes.foreign_loading);
}
