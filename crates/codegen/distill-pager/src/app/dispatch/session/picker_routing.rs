// Modified for Distill by Samuel Fajreldines, 2026.
//! Host routing for session picker fetch results: resolve the host a result was issued for to its live picker storage.
//! The target carries the freshness values (generation and per-kind seqs) the result must match to apply.

use crate::app::app_view::{AppView, SessionPickerEntry};
use crate::app::dispatch::ctx::get_active_agent_mut;
use crate::views::modal::ActiveModal;
use crate::views::picker::PickerState;
use crate::views::session_picker::{
    PickerSelectionAnchor, SessionPickerLanes, SessionPickerPendingNotice, SourceFilter,
    capture_picker_selection, effective_filter_query, repo_name_from_cwd, restore_picker_selection,
};
use crate::views::session_picker_surface::SessionPickerHost;

type SearchHit = distill_shell::extensions::session_search::SearchSessionHit;

/// A picker fetch carries this and its result echoes it back.
/// It names the requesting host and snapshots the generation and per-kind seq the host's picker had when the fetch dispatched.
#[derive(Debug, Clone, Copy)]
pub(in crate::app::dispatch) struct PickerRequest {
    pub host: SessionPickerHost,
    pub generation: u64,
    pub seq: u64,
}

/// Which of the target picker's seq counters a result kind is checked against.
pub(in crate::app::dispatch) enum PickerSeqKind {
    List,
    DeepSearch,
    Detail,
}

/// The accept rule shared by every picker result handler.
/// Resolve the requesting host to its live picker (drop when it has none), then require generation equality and the per-kind seq to still be current.
/// Returns the routed target on accept; logs and returns `None` on any failed check.
pub(in crate::app::dispatch) fn accept_picker_result<'a>(
    app: &'a mut AppView,
    request: PickerRequest,
    seq_kind: PickerSeqKind,
    kind: &'static str,
) -> Option<PickerTarget<'a>> {
    let PickerRequest {
        host,
        generation,
        seq,
    } = request;
    let Some(target) = picker_for_host(app, host) else {
        tracing::debug!(
            ?host,
            generation,
            seq,
            kind,
            "picker result for dead host dropped"
        );
        return None;
    };
    let live_seq = match seq_kind {
        PickerSeqKind::List => target.list_seq,
        PickerSeqKind::DeepSearch => target.deep_search_seq,
        PickerSeqKind::Detail => *target.detail_seq,
    };
    if generation != target.generation || seq != live_seq {
        tracing::debug!(
            ?host,
            generation,
            live_generation = target.generation,
            seq,
            live_seq,
            kind,
            "stale picker result dropped"
        );
        return None;
    }
    Some(target)
}

/// Mutable view of one host's picker storage plus the freshness values a result must match.
/// Generation and the list/deep-search seqs are by-value copies.
/// Result handlers only read them (producers bump the real counters before the handler borrows the storage).
pub(in crate::app::dispatch) struct PickerTarget<'a> {
    pub entries: &'a mut Option<Vec<SessionPickerEntry>>,
    pub loading: &'a mut bool,
    pub lanes: &'a mut SessionPickerLanes,
    pub state: &'a mut PickerState,
    pub content_results: &'a mut Option<Vec<SearchHit>>,
    pub content_loading: &'a mut bool,
    pub entries_query: &'a mut Option<String>,
    pub source_filter: SourceFilter,
    pub grouped: bool,
    pub current_repo: String,
    pub generation: u64,
    pub list_seq: u64,
    pub deep_search_seq: u64,
    pub detail_seq: &'a mut u64,
}

/// Resolve `host` to its live picker storage.
/// `None` means the host has no live picker (modal closed, dashboard unmounted) and the caller must drop the result; there is no fallback surface.
pub(in crate::app::dispatch) fn picker_for_host(
    app: &mut AppView,
    host: SessionPickerHost,
) -> Option<PickerTarget<'_>> {
    match host {
        SessionPickerHost::AgentModal => {
            // The welcome/modal list seq is one shared cell; the modal owns the other counters
            let list_seq = app.session_picker_list_seq;
            let agent = get_active_agent_mut(app)?;
            let current_repo = repo_name_from_cwd(&agent.session.cwd.to_string_lossy());
            let Some(ActiveModal::SessionPicker {
                state,
                entries,
                loading,
                lanes,
                content_results,
                content_loading,
                deep_search_seq,
                generation,
                detail_seq,
                entries_query,
                source_filter,
                ..
            }) = agent.active_modal.as_mut()
            else {
                return None;
            };
            Some(PickerTarget {
                entries,
                loading,
                lanes,
                state,
                content_results,
                content_loading,
                entries_query,
                source_filter: *source_filter,
                grouped: true,
                current_repo,
                generation: *generation,
                list_seq,
                deep_search_seq: *deep_search_seq,
                detail_seq,
            })
        }
        // Welcome storage always exists; staleness is caught by the generation check, so the accessor always resolves
        SessionPickerHost::Welcome => {
            let current_repo = repo_name_from_cwd(&app.cwd.to_string_lossy());
            Some(PickerTarget {
                entries: &mut app.session_picker_entries,
                loading: &mut app.session_picker_loading,
                lanes: &mut app.session_picker_lanes,
                state: &mut app.session_picker_state,
                content_results: &mut app.session_picker_content_results,
                content_loading: &mut app.session_picker_content_loading,
                entries_query: &mut app.session_picker_entries_query,
                source_filter: app.session_picker_source_filter,
                grouped: app.session_picker_grouped,
                current_repo,
                generation: app.session_picker_generation,
                list_seq: app.session_picker_list_seq,
                deep_search_seq: app.session_picker_deep_search_seq,
                detail_seq: &mut app.session_picker_detail_seq,
            })
        }
        SessionPickerHost::Dashboard => {
            let picker_cwd = app
                .dashboard
                .as_ref()
                .map_or(app.cwd.as_path(), |dashboard| dashboard.cwd.as_path());
            let current_repo = repo_name_from_cwd(&picker_cwd.to_string_lossy());
            let surface = app.dashboard_session_picker.as_mut()?;
            Some(PickerTarget {
                entries: &mut surface.entries,
                loading: &mut surface.loading,
                lanes: &mut surface.lanes,
                state: &mut surface.state,
                content_results: &mut surface.content_results,
                content_loading: &mut surface.content_loading,
                entries_query: &mut surface.entries_query,
                source_filter: surface.source_filter,
                grouped: true,
                current_repo,
                generation: surface.generation,
                list_seq: surface.list_seq,
                deep_search_seq: surface.deep_search_seq,
                detail_seq: &mut surface.detail_seq,
            })
        }
    }
}

impl PickerTarget<'_> {
    fn capture_selection(&self) -> PickerSelectionAnchor {
        capture_picker_selection(
            self.entries.as_deref(),
            self.content_results.as_deref(),
            self.state,
            effective_filter_query(self.state.query(), self.entries_query.as_deref()),
            self.grouped,
            *self.content_loading,
            self.source_filter,
            Some(&self.current_repo),
        )
    }

    fn restore_selection(&mut self, anchor: PickerSelectionAnchor) {
        let filter_query =
            effective_filter_query(self.state.query(), self.entries_query.as_deref()).to_owned();
        restore_picker_selection(
            anchor,
            self.entries.as_deref(),
            self.content_results.as_deref(),
            self.state,
            &filter_query,
            self.grouped,
            *self.content_loading,
            self.source_filter,
            Some(&self.current_repo),
        );
        self.state.expanded.clear();
    }

    pub(in crate::app::dispatch) fn native_loaded(
        &mut self,
        sessions: Vec<SessionPickerEntry>,
        query: Option<String>,
        chat_mode: bool,
        empty_notice: String,
        partial_notice: Option<&'static str>,
    ) -> Option<String> {
        let anchor = self.capture_selection();
        let is_search = query.is_some();
        *self.loading = false;
        if is_search {
            *self.content_loading = false;
        }
        *self.entries_query = query;
        if chat_mode {
            *self.entries = (!sessions.is_empty()).then_some(sessions);
        } else {
            crate::app::foreign_sessions::replace_native_entries(self.entries, sessions);
        }
        if (is_search || self.source_filter.is_active()) && self.entries.is_none() {
            *self.entries = Some(Vec::new());
        }
        let notice = if self.entries.is_none() && !is_search {
            if self.lanes.foreign_loading {
                self.lanes.pending_notice = Some(SessionPickerPendingNotice::Empty(empty_notice));
                None
            } else {
                self.lanes.pending_notice = None;
                Some(empty_notice)
            }
        } else {
            self.lanes.pending_notice = None;
            if chat_mode {
                partial_notice.map(str::to_owned)
            } else {
                None
            }
        };
        self.restore_selection(anchor);
        notice
    }

    pub(in crate::app::dispatch) fn native_failed(
        &mut self,
        error_notice: String,
        is_search: bool,
        chat_mode: bool,
    ) -> Option<String> {
        let anchor = self.capture_selection();
        *self.loading = false;
        if is_search {
            *self.content_loading = false;
        }
        if chat_mode {
            *self.entries = None;
        } else {
            crate::app::foreign_sessions::replace_native_entries(self.entries, Vec::new());
        }
        if self.source_filter.is_active() && self.entries.is_none() {
            *self.entries = Some(Vec::new());
        }
        *self.entries_query = None;
        let notice = if self.lanes.foreign_loading {
            self.lanes.pending_notice = Some(SessionPickerPendingNotice::Error(error_notice));
            None
        } else {
            self.lanes.pending_notice = None;
            Some(error_notice)
        };
        self.restore_selection(anchor);
        notice
    }

    pub(in crate::app::dispatch) fn foreign_loaded(
        &mut self,
        scanned: Vec<SessionPickerEntry>,
    ) -> Option<String> {
        let anchor = self.capture_selection();
        crate::app::foreign_sessions::replace_foreign_entries(self.entries, scanned);
        self.lanes.foreign_loading = false;
        let notice = self.lanes.take_ready_notice(self.entries.is_some());
        self.restore_selection(anchor);
        notice
    }
}
