// Modified for Distill by Samuel Fajreldines, 2026.
//! Dashboard state types: `DashboardState`, `DashboardRowId`, filters, grouping, persistence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use super::animation::PaintedAnimations;
use super::peek::PeekPanelState;
use super::row::DashboardRow;
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::agent::AgentId;
use crate::app::app_view::InputOutcome;
use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::key;
use crate::views::prompt_widget::{PromptEvent, PromptWidget};
use distill_shell::session::persistence::MAX_TITLE_SCALARS as MAX_RENAME_SCALARS;

const PROMPT_MULTI_CLICK_MS: u128 = 300;

/// Stable identity for a dashboard row. When the parent closes, the subagent rows naturally
/// disappear because the row builder iterates `app.agents.values()`; no separate cleanup needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DashboardRowId {
    TopLevel(AgentId),
    Subagent {
        parent: AgentId,
        child_session_id: String,
    },
    /// A leader-roster-only row: a session hosted by the leader (or a remote host) that this client is NOT locally attached to.
    /// Keyed by the roster `session_id`.
    /// Not locally controllable.
    Roster {
        session_id: String,
    },
    Workspace {
        session_id: String,
    },
}

impl DashboardRowId {
    /// True for a subagent row.
    /// Used to gate the "rename" affordance; subagents are read-only in this version.
    pub fn is_subagent(&self) -> bool {
        matches!(self, Self::Subagent { .. })
    }

    pub fn is_workspace(&self) -> bool {
        matches!(self, Self::Workspace { .. })
    }

    pub(crate) fn matches_top_level_agent(&self, agent_id: AgentId) -> bool {
        matches!(self, Self::TopLevel(id) if *id == agent_id)
    }

    pub(crate) fn is_owned_by_agent(&self, agent_id: AgentId) -> bool {
        matches!(self, Self::TopLevel(id) if *id == agent_id)
            || matches!(self, Self::Subagent { parent, .. } if *parent == agent_id)
    }
}

pub(crate) struct PeekViewportLease {
    pub row: DashboardRowId,
    pub snapshot: crate::scrollback::state::ViewportSnapshot,
    pub page_flip_entry: Option<crate::scrollback::EntryId>,
}

pub(crate) fn scrollback_mut_for_row<'a>(
    row: &DashboardRowId,
    agents: &'a mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
) -> Option<&'a mut crate::scrollback::state::ScrollbackState> {
    match row {
        DashboardRowId::TopLevel(id) => agents.get_mut(id).map(|a| &mut a.scrollback),
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get_mut(parent)
            .and_then(|p| p.subagent_view_mut(child_session_id))
            .map(|c| &mut c.scrollback),
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => None,
    }
}

pub(crate) fn scrollback_available_for_row(
    row: &DashboardRowId,
    agents: &indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
) -> bool {
    match row {
        DashboardRowId::TopLevel(id) => agents.contains_key(id),
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get(parent)
            .is_some_and(|p| p.has_subagent_view(child_session_id)),
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => false,
    }
}

/// A dispatch-input send (spawn a new session) stashed while a clipboard attachment probe is
/// off-thread.
#[derive(Debug)]
pub(crate) struct DeferredDispatchSend {
    pub(crate) attach: bool,
}

/// A peek-reply send (reply to an existing agent) stashed while a clipboard attachment probe is off-thread.
/// `row` pins the reply's target so a peek that closed or moved to another row drops the stash instead of replying to the wrong agent.
#[derive(Debug)]
pub(crate) struct DeferredPeekSend {
    pub(crate) row: DashboardRowId,
    pub(crate) attach: bool,
}

/// Persistent identity for a pinned/reordered row. Rows whose underlying session has not yet been
/// created (and thus have no `session_id`) cannot be persisted; they survive only in-memory across
/// the current process lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PersistedRowId {
    TopLevel {
        session_id: String,
    },
    Subagent {
        parent_session_id: String,
        child_session_id: String,
    },
}

impl PersistedRowId {
    /// On-disk serialisation. `top:<session_id>` or `sub:<parent_session_id>:<child_session_id>`. The
    /// session ids themselves are opaque to the dashboard; we never split them. The first colon after
    /// `sub:` is the only one we split on.
    pub fn to_key(&self) -> String {
        match self {
            Self::TopLevel { session_id } => format!("top:{session_id}"),
            Self::Subagent {
                parent_session_id,
                child_session_id,
            } => format!("sub:{parent_session_id}:{child_session_id}"),
        }
    }

    /// Parse a persisted key back.
    /// Returns `None` for malformed input.
    pub fn from_key(s: &str) -> Option<Self> {
        if let Some(sid) = s.strip_prefix("top:") {
            if sid.is_empty() {
                return None;
            }
            return Some(Self::TopLevel {
                session_id: sid.to_string(),
            });
        }
        if let Some(rest) = s.strip_prefix("sub:")
            && let Some((parent, child)) = rest.split_once(':')
            && !parent.is_empty()
            && !child.is_empty()
        {
            return Some(Self::Subagent {
                parent_session_id: parent.to_string(),
                child_session_id: child.to_string(),
            });
        }
        None
    }
}

/// Window within which a second confirming gesture (`Ctrl+X`, a `[✗]` click, or `y`) deletes the armed row.
/// Also reused by the dashboard-overlay stop for its double-press close confirm.
pub const CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// How long after a send an empty Enter still counts as an echo of that send.
/// One second covers the default key auto-repeat delay on macOS (375 ms), Windows (500 ms), and GNOME (500 ms).
const SEND_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn send_echo_window_open(since_send: std::time::Duration) -> bool {
    since_send < SEND_ECHO_WINDOW
}

/// Coarse state used for the dashboard grouping.
///
/// See [`super::row::classify_top_level`] / [`super::row::classify_subagent`] for the mapping rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum RowState {
    /// Pending permission OR pending ask_user_question (top-level only; subagents never enter this state in this version).
    NeedsInput,
    /// Live turn or command running.
    Working,
    /// Alive, idle.
    Idle,
    /// A roster-only session: idle / dormant in another pager process and never loaded in this one.
    /// Local agents never classify as `Inactive` (see `classify_top_level`); only `roster_activity_to_state` produces it, so the "Idle" section
    /// stays focused on sessions you're actively cycling between.
    Inactive,
    /// Finished, with status == "completed".
    Completed,
    /// Finished, with status in ("failed", "cancelled").
    Failed,
}

impl RowState {
    /// Only settled rows may be deleted; working and waiting-for-user rows must remain intact.
    pub fn allows_delete(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Inactive | Self::Completed | Self::Failed
        )
    }

    /// Sort priority used inside a state group: higher = floats up.
    /// Pinned rows always float to the absolute top regardless of state.
    pub fn group_priority(self) -> u8 {
        match self {
            Self::NeedsInput => 6,
            Self::Working => 5,
            Self::Idle => 3,
            // Below Idle (these aren't loaded here, so they're less immediately
            // actionable) but above Done/Failed (they're still live, resumable sessions)
            Self::Inactive => 2,
            Self::Completed => 1,
            Self::Failed => 0,
        }
    }

    /// Human-readable group header.
    pub fn group_label(self) -> &'static str {
        match self {
            // Shorter, punchier labels
            // "Done" reads cleaner as a group header than the past-tense "Completed" did
            Self::NeedsInput => "Awaiting",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Inactive => "Inactive",
            Self::Completed => "Done",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DashboardStopAction {
    Stop,
    Archive,
    Close,
}

impl DashboardStopAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Archive => "archive",
            Self::Close => "close",
        }
    }

    pub(crate) fn confirmation_label(self) -> Option<&'static str> {
        match self {
            Self::Stop => None,
            Self::Archive => Some("archive this session"),
            Self::Close => Some("close this session"),
        }
    }
}

/// Stable identity for a collapsible/selectable dashboard section header.
/// Sections only exist in [`Grouping::State`] mode: the cross-cutting "Pinned" block plus one section per [`RowState`] group.
/// Keyed by the stable group identity (not a positional index) so collapse / selection survive re-sorting and row churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKey {
    /// The "Pinned" block header.
    Pinned,
    /// A per-state group header (Working / Awaiting / Idle / …).
    State(RowState),
}

pub use crate::views::dashboard::actions_focus::ActionsFocus;
pub(crate) use crate::views::dashboard::actions_focus::Step;

/// A keyboard-navigable cursor target in the dashboard list: a collapsible section header or a row.
/// Built in display order (see `render::focusables`) so Up/Down navigation and the section/row cursor stay in lockstep with what the renderer paints.
/// A collapsed section contributes only its header (its rows are absent), so nav skips hidden rows.
#[derive(Debug, Clone, PartialEq)]
pub enum Focusable {
    Section(SectionKey),
    Row(DashboardRowId),
    /// The Idle group's "N more" toggle row.
    /// Present only when the Idle group is capped (more than `MAX_VISIBLE_IDLE` agents, the overflow older than the freshness window).
    /// Activating it (Enter / click) flips [`DashboardState::idle_show_all`].
    IdleOverflow,
}

/// Grouping mode for the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// Group by `RowState` (the default).
    #[default]
    State,
    /// Group by working directory (one section per cwd).
    Directory,
}

impl Grouping {
    pub fn toggled(self) -> Self {
        match self {
            Self::State => Self::Directory,
            Self::Directory => Self::State,
        }
    }
}

impl From<crate::app::workspace_layout::WorkspaceGrouping> for Grouping {
    fn from(grouping: crate::app::workspace_layout::WorkspaceGrouping) -> Self {
        match grouping {
            crate::app::workspace_layout::WorkspaceGrouping::State => Self::State,
            crate::app::workspace_layout::WorkspaceGrouping::Directory => Self::Directory,
        }
    }
}

/// A filter for the visible rows. Parsed from the dispatch input; see [`parse_filter`]. The
/// dispatcher applies it in [`super::row::apply_filter`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    /// Show everything.
    #[default]
    None,
    /// Match by agent label (`a:<name>`).
    /// Case-insensitive substring.
    Agent(String),
    /// Match by row state (`s:<state>`).
    State(RowState),
    /// Free-text substring match against label and cwd display.
    Substring(String),
}

impl Filter {
    pub fn from_value(v: FilterValue) -> Self {
        match v {
            FilterValue::None => Self::None,
            FilterValue::Agent(s) => {
                if s.trim().is_empty() {
                    Self::None
                } else {
                    Self::Agent(s)
                }
            }
            FilterValue::State(rs) => Self::State(rs),
            FilterValue::Substring(s) => {
                if s.trim().is_empty() {
                    Self::None
                } else {
                    Self::Substring(s)
                }
            }
        }
    }

    /// Whether this filter would hide any rows.
    /// Used to decide whether `Esc` should clear the filter before exiting.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Transport type for the [`Action::DashboardSetFilter`] action.
/// Kept separate from [`Filter`] so the action stays `Debug + Clone` without requiring `Filter` to be `Hash`.
#[derive(Debug, Clone)]
pub enum FilterValue {
    None,
    Agent(String),
    State(RowState),
    Substring(String),
}

/// Persisted dashboard configuration stored under `[dashboard]` in `~/.grok/config.toml`. Lenient:
/// corrupted fields fall back to defaults.
#[derive(Debug, Clone, Default)]
pub struct PersistedDashboard {
    pub enabled: bool,
    pub grouping: Grouping,
    pub pinned: BTreeSet<PersistedRowId>,
    pub reorder: Vec<PersistedRowId>,
}

impl PersistedDashboard {
    /// Construct a `PersistedDashboard` with feature-flag defaults.
    pub fn defaults() -> Self {
        Self {
            enabled: true,
            grouping: Grouping::default(),
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
        }
    }
}

/// In-memory dashboard state. Refreshed every render frame off `app.agents`. Selection is keyed by
/// `DashboardRowId` so a rename / reorder / completion does not invalidate the cursor as long as
/// the row's id is stable.
pub struct DashboardState {
    /// Currently selected row id.
    /// May be `None` when no rows are visible.
    pub selected: Option<DashboardRowId>,
    /// Hover target for visual feedback.
    pub hovered_row: Option<DashboardRowId>,
    /// Section-header cursor target.
    /// When `Some`, a collapsible section title (e.g. "Working") holds the cursor instead of a row or the `+ New Agent` button.
    /// Mutually exclusive with [`Self::selected`] and [`Self::actions_focus`].
    pub selected_section: Option<SectionKey>,
    /// Hovered section header (mouse-move driven); the renderer brightens its text.
    /// Independent of [`Self::hovered_row`].
    pub hovered_section: Option<SectionKey>,
    /// Sections the user has collapsed (their rows are hidden).
    /// In-memory for the dashboard's lifetime; keyed by stable [`SectionKey`].
    pub collapsed_sections: std::collections::HashSet<SectionKey>,
    /// Cursor sits on the Idle group's "N more" overflow toggle row.
    /// The fourth cursor target; mutually exclusive with [`Self::selected`], [`Self::selected_section`], and [`Self::actions_focus`]
    /// (enforced via the `focus_*` helpers).
    pub selected_idle_overflow: bool,
    /// Mouse is hovering the Idle overflow toggle row; the renderer brightens its text.
    /// Independent of [`Self::hovered_row`].
    pub hovered_idle_overflow: bool,
    /// When `true`, the Idle group shows every agent; when `false` (default) it caps to the `MAX_VISIBLE_IDLE` most-recent agents plus anything still
    /// inside the freshness window, folding the rest behind the overflow row.
    /// In-memory only (resets on pager restart), like section collapse.
    pub idle_show_all: bool,
    /// V1-only config layout; dashboard v2 derives these fields from `WorkspaceView`.
    pub pinned: BTreeSet<DashboardRowId>,
    pub reorder: Vec<DashboardRowId>,
    pub grouping: Grouping,
    /// Last v2 grouping observed by the renderer; transition detection only.
    workspace_grouping: Option<Grouping>,
    pub filter: Filter,
    /// Search mode (toggled by `Ctrl+/`). While active the [`Self::dispatch`] buffer is interpreted as
    /// a live filter query rather than a prompt: every keystroke reparses into.
    pub search_mode: bool,
    /// Dispatch / filter input widget.
    pub dispatch: PromptWidget,
    /// Effects queued by dashboard input (e.g. slash MRU persist after Tab accept).
    pub(crate) pending_effects: Vec<crate::app::actions::Effect>,
    /// In-flight deferred clipboard attachment probes for this dashboard.
    /// A send while `> 0` is stashed per surface so a paste-then-immediate-send never builds content blocks before the image attaches.
    pub(crate) paste_probe_in_flight: usize,
    /// A dispatch-input send deferred until the in-flight paste probe(s) complete; re-issued (with the now-updated widget text) on completion.
    pub(crate) deferred_dispatch_send: Option<DeferredDispatchSend>,
    /// A peek-reply send deferred the same way; per-surface slots so stashing one surface can never overwrite the other's pending send.
    pub(crate) deferred_peek_send: Option<DeferredPeekSend>,
    /// When the dispatch input last sent its text.
    /// `AppView` clears it on any key other than Enter.
    pub(crate) last_send_at: Option<Instant>,
    /// Peek panel state (Space toggles).
    pub peek: Option<PeekPanelState>,
    /// Session-scoped guest viewport for the live-tail peek (capture once on select; sticky while the same row is peeked; restore on leave).
    pub(crate) peek_viewport: Option<PeekViewportLease>,
    /// Owned here (NOT inside [`PeekPanelState`]) because a widget carries a fuzzy-file-matcher daemon
    /// thread and the panel struct is rebuilt whenever the selection cursor lands on a row.
    pub peek_reply: PromptWidget,
    /// Last frame's screen rect of the peek reply input (the `❯ reply` row, or the reject-feedback slot in question mode).
    /// Mirrors [`Self::dispatch_rect`]: consumed by mouse handling for click-to-focus and drag text selection.
    /// `None` while the peek is closed.
    pub peek_reply_rect: Option<Rect>,
    /// Directory the reply's `@` file-search daemon is currently rooted at. Tracked so
    /// [`Self::ensure_peek_reply_cwd`] can skip a `retarget` (which drops the daemon so the next @-use
    /// rebuilds it) when the peeked agent's cwd hasn't actually changed.
    peek_reply_cwd: Option<PathBuf>,
    /// Cwd of the currently-peeked agent, recorded by the render pass (which has the agents map).
    peek_reply_target_cwd: Option<PathBuf>,
    /// Inline-rename state for `Ctrl+R`: `Some` when the cursor is on a row and the user is mid-edit.
    pub rename: Option<RenameDraft>,
    /// Pending dispatch feedback toast (e.g. "✗ Session no longer exists").
    /// Rendered verbatim by `paint_dispatch_feedback_badge`; error messages are built via [`Self::set_error_toast`].
    pub error_toast: Option<String>,
    /// Row armed for delete, and when.
    /// A second gesture on the same row within [`CONFIRM_WINDOW`] deletes it (see [`Self::armed_delete_row`]); otherwise it lapses.
    /// Cleared on any focus change.
    pub delete_confirm: Option<(DashboardRowId, Instant)>,
    pub workspace_membership_mode: bool,
    /// Ctrl+X meaning for the selected v2 row in the current frame.
    pub(crate) selected_stop_action: Option<DashboardStopAction>,
    pub spinner_tick: u64,
    pub(crate) painted_animations: PaintedAnimations,
    /// Last frame's row layout: hit areas keyed by row id.
    /// Used by mouse handling to map (col, row) to a row id without scanning the row list a second time.
    pub row_rects: Vec<(DashboardRowId, Rect)>,
    /// Per-row `[✗]` hit areas, rebuilt each render; maps a click onto the delete gesture instead of a row select.
    pub row_delete_rects: Vec<(DashboardRowId, Rect)>,
    /// Row whose `[✗]` the mouse is over, so the renderer can tint it.
    pub hovered_delete: Option<DashboardRowId>,
    /// Roster session ids whose origin is a chat `conversation`; those can't be deleted from the dashboard yet, so they get no `[✗]` and don't arm.
    /// Rebuilt each render from the roster.
    pub conversation_row_ids: std::collections::HashSet<String>,
    /// Last frame's section-header hit areas keyed by [`SectionKey`].
    /// Used by mouse handling to map (col, row) to a section for click-to-toggle and hover.
    /// Rebuilt every render.
    pub section_rects: Vec<(SectionKey, Rect)>,
    /// Last frame's hit area for the Idle "N more" overflow toggle row, when painted.
    /// Consumed by mouse handling for click-to-toggle and hover.
    /// `None` when the Idle group isn't capped.
    pub idle_overflow_rect: Option<Rect>,
    /// Last frame's content-area rect for resize-aware peek toggling.
    pub last_area: Rect,
    /// When `Some(id)`, an agent conversation is attached as a popup overlay on top of the dashboard.
    pub attached_agent: Option<AgentId>,
    /// Hit rect for the popup's `[✗]` close affordance.
    /// Populated by the renderer when the popup chrome is painted; consumed by `handle_mouse` to dispatch a popup close on click.
    /// `None` when no popup is open or the popup is too small for the affordance.
    pub popup_close_rect: Option<Rect>,
    /// Outer rect of the popup overlay (border included). Populated by the renderer; consumed by
    /// `handle_mouse` to.
    pub popup_outer_rect: Option<Rect>,
    /// Hit area for the actions row's `+ New Agent` button.
    pub new_agent_button_hit: crate::app::agent_view::HitArea,
    /// Hit area for the actions row's `Open Previous` button (v2 workspace dashboard only).
    pub open_session_button_hit: crate::app::agent_view::HitArea,
    /// Hit area for the actions row's `Worktree Ctrl+w` hint; a click toggles worktree mode like the chord does.
    pub worktree_toggle_hit: crate::app::agent_view::HitArea,
    /// Items-area rect of the open slash-completion dropdown (set by `render_slash_dropdown`).
    /// Lets the mouse handler route a wheel / click inside the dropdown to the completion list rather than the row list.
    /// `None` when the dropdown is closed.
    pub slash_dropdown_items_area: Option<Rect>,
    /// Slash dropdown hit-test state from the renderer (see [`crate::views::slash_dropdown::RenderedDropdown`]).
    pub slash_dropdown_hit: crate::views::slash_dropdown::RenderedDropdown,
    /// Mirror of [`Self::slash_dropdown_items_area`] for the session-less `@` file-context picker dropdown (set by `render_file_search_dropdown`).
    pub file_search_dropdown_items_area: Option<Rect>,
    /// Two-focus model: `false` means the dispatch input bar is focused (typing); `true` means the
    /// overview list is focused (navigating).
    pub list_focused: bool,
    /// Last mouse position (col, row).
    /// Used by hover and double-click detection.
    pub last_mouse_pos: Option<(u16, u16)>,
    /// Last mouse-down on a row, with timestamp, used to detect a double-click as the "attach" gesture.
    pub last_click: Option<(DashboardRowId, Instant)>,
    /// Last mouse-down on dispatch / peek-reply (paste-chip double-click).
    pub last_prompt_click: Option<Instant>,
    /// Rect that closes the peek when clicked (`[×]`).
    pub peek_close_rect: Option<Rect>,
    /// Outer rect of the dispatch input box (set by `render_dashboard` when the box is painted).
    /// Consumed by the mouse handler so a click anywhere on the box focuses the input (i.e. clears `list_focused`) regardless of vim mode.
    /// `None` while peek mode or an attached-agent overlay replaces the input.
    pub dispatch_rect: Option<Rect>,
    /// Top of the visible row window.
    /// Clamped so the selected row stays visible.
    /// Wheel and PgUp/PgDn adjust this.
    pub viewport_offset: usize,
    /// True while the user is browsing the row list via the mouse wheel (or any pure-viewport scroll affordance).
    /// When set, [`Self::clamp_viewport`] skips its snap-to-selection pull-back so the viewport can travel past the selected row.
    /// Cleared on any selection-driven nav (arrow keys, click, filter rebuild, attach) so the viewport re-engages the selection follow.
    pub manual_scroll_active: bool,
    /// `Ctrl+.` opens a searchable cheatsheet of every action bound for the dashboard, mirroring the agent view's `ShortcutsHelp` modal.
    /// `Some` while the modal is open; input is routed to it before the dashboard's own handlers, and the renderer paints it on top of the row list.
    /// Cleared on close (Esc, `[✗]`, or the chrome's CloseRequested).
    pub shortcuts_modal: Option<Box<ShortcutsModalState>>,
    /// Which actions-row item holds the keyboard cursor, if any.
    /// `Some(NewAgent)` is the default target when no row is selected; Up-arrow from the first row, Esc deselect, and
    /// dashboard-open-without-prior-agent all land there.
    /// Mutually exclusive with every row/section cursor: at most one of `selected`, `selected_section`, `selected_idle_overflow`,
    /// and this is set (row churn can leave all four clear). Write it through the `focus_*` helpers so that invariant holds.
    pub actions_focus: Option<ActionsFocus>,
    /// Model chosen for the next agent spawned from the dispatch input, set by `/model <name> [effort]`
    /// (intercepted in `dispatch_dashboard_dispatch_slash`). `None` spawns on the default model. Sticky
    /// across dispatches; reset to `None` on every dashboard-open (alongside `pending_mode`).
    pub pending_model: Option<PendingDispatchModel>,
    /// Mode the next spawned agent starts in.
    /// Cycled with Shift+Tab and set by `/plan`.
    /// Sticky across dispatches; re-seeded from `app.default_yolo` on every dashboard-open (alongside `pending_model`).
    pub pending_mode: DashboardDispatchMode,
    /// Snapshot of the app-wide model catalog, seeded at dashboard-open so the session-less slash dropdown can suggest real model names for `/model`.
    /// Without this the dropdown would fall back to an empty `ModelState` and offer no completions.
    pub models: crate::acp::model_state::ModelState,
    /// Location picker modal: `Some` while open.
    pub location_picker: Option<LocationPickerState>,
    /// Hit area for the header's clickable location label.
    /// Painted by `render_header` and consumed by the mouse handler to open the location picker.
    /// `None` when the header is too narrow to paint it.
    pub location_hit: crate::app::agent_view::HitArea,
    /// Hit area for the header's promo upgrade CTA `[label]` button (a click dispatches `AnnouncementsOpenCta(Dashboard)`).
    /// `None` when no CTA is shown.
    pub upgrade_cta_hit: crate::app::agent_view::HitArea,
    /// A pinned (non-dismissible) promo CTA is live this frame (cached by `render_dashboard`); `Ctrl+O` opens it instead of falling through.
    pub pinned_upgrade_cta_live: bool,
    /// When `true`, agents dispatched from the dashboard are created in a fresh git worktree (rooted at
    /// the current cwd) instead of in the cwd directly: creating an agent first opens
    /// [`Self::worktree_dialog`] to collect a label.
    pub dispatch_worktree: bool,
    /// Whether the current working directory is inside a git repo. Worktrees require a git repo, so
    /// this gates the `Ctrl+W` worktree toggle and forces [`Self::dispatch_worktree`] off outside a
    /// repo (the dashboard is never in worktree mode in a non-git directory).
    pub cwd_has_git_ancestor: bool,
    /// Working directory new sessions dispatch from: a snapshot of `AppView::cwd`, synced on dashboard-open and on every location change.
    /// The header renders from this (not the process cwd) so it tracks a `/cd` even before `Effect::SetWorkingDir` lands, or if it fails.
    pub cwd: PathBuf,
    /// Worktree-label dialog: `Some` while the user is naming a worktree for a dashboard-dispatched agent.
    /// Reuses the welcome screen's [`NewWorktreeDialogState`](crate::app::app_view::NewWorktreeDialogState) widget; input is
    /// routed here and the renderer overlays it.
    pub worktree_dialog: Option<crate::app::app_view::NewWorktreeDialogState>,
    /// Prompt state stashed while [`Self::worktree_dialog`] is open.
    pub pending_worktree_prompt: Option<crate::views::prompt_widget::StashedPrompt>,
    /// Whether confirming the in-flight [`Self::worktree_dialog`] should open the new agent's detail
    /// view (`true`; the `[+ New Agent]` button and `Ctrl+S` "send and open") or stay on the dashboard
    /// (`false`; a plain `Enter` prompt-send).
    pub pending_worktree_attach: bool,
    /// Mirror of `AppView::voice_listening`, synced each frame so the dispatch box can show a record badge and stream the interim transcript while
    /// voice dictation targets the dashboard's new-agent input.
    pub voice_listening: bool,
    /// Mirror of `AppView::voice_interim`: the live partial transcript.
    pub voice_interim: Option<String>,
    /// Surface-local compose mode for dispatch and peek (not persisted; not shared with agent sessions).
    /// `/multiline` or Ctrl+M.
    pub multiline_mode: bool,
    /// `/usage` modal, hosted here because the dashboard has no agent to hang it on (session-less: no session id).
    /// Owns input while open; cleared on dashboard-open and on every overlay exit back to the list.
    pub usage_modal: Option<Box<crate::views::usage_modal::UsageInfoModalState>>,
}

/// Mode staged for the next agent the dashboard spawns.
/// Mirrors the agent view's Shift+Tab cycle (Normal → Plan → Auto → Always-Approve → Normal when Auto is enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardDispatchMode {
    #[default]
    Normal,
    Plan,
    Auto,
    AlwaysApprove,
}

impl DashboardDispatchMode {
    /// Advance to the next mode in the Shift+Tab rotation.
    pub fn cycle(self, auto_mode_gate: bool) -> Self {
        match self {
            Self::Normal => Self::Plan,
            Self::Plan if auto_mode_gate => Self::Auto,
            Self::Plan | Self::Auto => Self::AlwaysApprove,
            Self::AlwaysApprove => Self::Normal,
        }
    }
}

/// A model (and optional reasoning effort) staged for the next agent the dashboard spawns.
/// `display` is the human-readable model name, stored so the renderer can show the indicator without a live `ModelState`.
#[derive(Debug, Clone)]
pub struct PendingDispatchModel {
    pub id: agent_client_protocol::ModelId,
    pub effort: Option<distill_shell::sampling::types::ReasoningEffort>,
    pub display: String,
}

/// No `Debug` / `Clone` derive; `ShortcutsHelpEntry` lacks both, and this state is owned in-place
/// by `DashboardState` without cloning anywhere.
pub struct ShortcutsModalState {
    pub entries: Vec<crate::views::shortcuts_help::ShortcutsHelpEntry>,
    pub state: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    pub filter_active: bool,
    pub collapsed_sections: std::collections::HashSet<usize>,
    pub expanded_ids: std::collections::HashSet<crate::views::shortcuts_help::ExpandKey>,
    pub mode: crate::views::shortcuts_help::ShortcutsHelpMode,
}

/// In-progress rename for `Ctrl+R`.
#[derive(Debug, Clone)]
pub struct RenameDraft {
    pub row: DashboardRowId,
    editor: LineEditor,
}

impl RenameDraft {
    pub fn new(row: DashboardRowId, text: impl Into<String>) -> Self {
        let mut draft = Self {
            row,
            editor: LineEditor::default(),
        };
        draft.set_text(text);
        draft
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn cursor_byte(&self) -> usize {
        self.editor.cursor_byte()
    }

    pub(crate) fn viewport(&self, width: usize) -> distill_ratatui_textarea::SingleLineViewport {
        self.editor.viewport(width)
    }

    pub(crate) fn set_text(&mut self, text: impl Into<String>) {
        let text = text
            .into()
            .chars()
            .filter(|character| rename_wire_character_allowed(*character))
            .take(MAX_RENAME_SCALARS)
            .collect::<String>();
        self.editor.set_text(text);
    }
}

fn rename_character_allowed(character: char) -> bool {
    !crate::render::line_utils::is_unsafe_display_char(character)
}

fn rename_wire_character_allowed(character: char) -> bool {
    // Preserve an existing emoji ZWJ sequence; interactive inserts still reject format chars.
    character == '\u{200d}' || rename_character_allowed(character)
}

/// One selectable directory in the location picker (see [`LocationPickerState`]).
#[derive(Debug, Clone)]
pub struct LocationCandidate {
    pub path: PathBuf,
    pub label: String,
    pub detail: String,
    pub worktree: Option<String>,
}

/// Max directory entries listed per parent (bounds the cost of a `readdir` on a huge directory) and max rows shown to the picker.
const LOCATION_DIR_LISTING_CAP: usize = 1000;
const LOCATION_VISIBLE_CAP: usize = 200;

/// State for the dashboard's location picker modal (change the directory new sessions spawn in).
pub struct LocationPickerState {
    pub picker: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    /// Static suggestions shown when the query isn't a path: current cwd first, then recent project dirs.
    pub recents: Vec<LocationCandidate>,
    /// Base directory for resolving relative typed paths (the pager cwd at open time).
    pub base_cwd: PathBuf,
    /// Index from worktree root path to label, built once at open.
    /// Used to tag recents and directory suggestions that are managed worktrees.
    worktrees: std::collections::HashMap<PathBuf, String>,
    /// Cached subdirectories of the current path-mode parent.
    dir_listing: Vec<LocationCandidate>,
    /// The parent directory [`Self::dir_listing`] was read from.
    /// Used to skip the `readdir` when only the final (partial) segment changes.
    dir_listing_parent: Option<PathBuf>,
    /// Content-row hit areas from the last render, for mouse click / hover.
    /// `None` until the modal renders once.
    pub content_hits: Option<crate::views::picker::PickerContentHitAreas>,
    /// Inline error (e.g. "Not a directory") shown under the title when a chosen path fails to resolve.
    /// Cleared when the modal re-opens.
    pub error: Option<String>,
    /// When `true`, applying the chosen location also arms worktree mode on the dashboard (`DashboardState::dispatch_worktree`), so the next
    /// dispatched agent spawns in a fresh worktree.
    /// Toggled by the path-row "worktree" button; seeded from the dashboard flag on open.
    pub worktree_mode: bool,
    /// Hit area (rect and hover) of the path-row "worktree" toggle button.
    /// The rect is set during render (cleared when the button is hidden; modal too narrow, or the target isn't a repo); `hovered` is driven by the
    /// modal mouse handler so the button can brighten on hover.
    pub worktree_hit: crate::app::agent_view::HitArea,
    /// Memoized git-repo check for the worktree toggle: `(dir, is_repo)`.
    /// Avoids re-`stat`ing on every render frame while the selection / query (and thus the target directory) is unchanged.
    worktree_repo_cache: Option<(PathBuf, bool)>,
}

impl LocationPickerState {
    /// Build a fresh picker over the static `recents` list, resolving relative typed paths against `base_cwd`.
    /// `worktrees` is the root-path-to-label index used to tag worktree directories.
    /// The search field is active so the cursor shows immediately and a path can be typed.
    pub fn new(
        recents: Vec<LocationCandidate>,
        base_cwd: PathBuf,
        worktrees: std::collections::HashMap<PathBuf, String>,
    ) -> Self {
        let picker = crate::views::picker::PickerState::input_active();
        Self {
            picker,
            window: crate::views::modal_window::ModalWindowState::new(),
            recents,
            base_cwd,
            worktrees,
            dir_listing: Vec::new(),
            dir_listing_parent: None,
            content_hits: None,
            error: None,
            worktree_mode: false,
            worktree_hit: crate::app::agent_view::HitArea::default(),
            worktree_repo_cache: None,
        }
    }

    /// Whether `dir` is inside a git repo (worktrees require one), memoized so navigating the list doesn't re-`stat` on every render frame.
    pub fn target_is_repo(&mut self, dir: &Path) -> bool {
        if let Some((cached, is_repo)) = &self.worktree_repo_cache
            && cached == dir
        {
            return *is_repo;
        }
        let is_repo = dir.ancestors().any(|p| p.join(".git").exists());
        self.worktree_repo_cache = Some((dir.to_path_buf(), is_repo));
        is_repo
    }

    /// Whether the current query should be treated as a filesystem path (directory completion) rather than a fuzzy filter over recents.
    pub fn query_is_path(&self) -> bool {
        let q = self.picker.query();
        q.starts_with('/')
            || q.starts_with('~')
            || q.contains('/')
            // Windows: native absolute paths (`C:\…`) and backslash separators
            // Gated so a literal backslash in a Unix filename never forces path mode on non-Windows hosts
            || (cfg!(windows) && (q.contains('\\') || has_windows_drive_prefix(q)))
    }

    /// Split a path-mode query into the `(parent_dir, partial_name)` to complete: everything up to the
    /// last separator resolves to a directory, and the trailing segment is the prefix to match.
    fn path_query_parts(&self) -> (PathBuf, String) {
        let q = self.picker.query();
        // Last path separator: `/` always; `\` additionally on Windows.
        let sep = match (q.rfind('/'), cfg!(windows).then(|| q.rfind('\\')).flatten()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        match sep {
            Some(i) => {
                let parent = resolve_dir_prefix(q.get(..=i).unwrap_or(q), &self.base_cwd);
                (parent, q.get(i + 1..).unwrap_or("").to_owned())
            }
            // No separator: a bare `~` (or `~name`) lists home; on Windows a bare drive (`C:`) lists that drive's root
            None => {
                if cfg!(windows) && has_windows_drive_prefix(q) && q.len() == 2 {
                    (PathBuf::from(format!("{q}\\")), String::new())
                } else {
                    let home = distill_dirs::home_dir().unwrap_or_else(|| self.base_cwd.clone());
                    (home, q.trim_start_matches('~').to_string())
                }
            }
        }
    }

    /// Re-`readdir` the path-mode parent when it changes.
    /// Cheap no-op when the query isn't a path or the parent is unchanged (typing within a directory only re-filters the cached listing).
    pub fn refresh_suggestions(&mut self) {
        if !self.query_is_path() {
            if self.dir_listing_parent.is_some() {
                self.dir_listing.clear();
                self.dir_listing_parent = None;
            }
            return;
        }
        let (parent, _partial) = self.path_query_parts();
        if self.dir_listing_parent.as_deref() == Some(parent.as_path()) {
            return;
        }
        self.dir_listing = read_subdirs(&parent, &self.worktrees);
        self.dir_listing_parent = Some(parent);
        self.picker.selected = 0;
        self.picker.scroll_offset = None;
    }

    /// The effective list shown and selected from, given the current query. In path mode, the cached
    /// subdirs prefix-matched against the partial (dot-directories hidden unless the partial starts
    /// with `.`); in recents mode, the static list fuzzy-filtered by substring.
    pub fn visible_candidates(&self) -> Vec<LocationCandidate> {
        if self.query_is_path() {
            let (_parent, partial) = self.path_query_parts();
            let pl = partial.to_lowercase();
            let want_hidden = partial.starts_with('.');
            self.dir_listing
                .iter()
                .filter(|c| {
                    if !want_hidden && c.label.starts_with('.') {
                        return false;
                    }
                    pl.is_empty() || c.label.to_lowercase().starts_with(&pl)
                })
                .take(LOCATION_VISIBLE_CAP)
                .cloned()
                .collect()
        } else {
            let q = self.picker.query().trim().to_lowercase();
            self.recents
                .iter()
                .filter(|c| {
                    q.is_empty()
                        || c.label.to_lowercase().contains(&q)
                        || c.detail.to_lowercase().contains(&q)
                        || c.path.to_string_lossy().to_lowercase().contains(&q)
                })
                .take(LOCATION_VISIBLE_CAP)
                .cloned()
                .collect()
        }
    }

    /// Resolve the path to apply on Enter: the selected visible row, else the raw typed
    /// query (so a path with no matching suggestion still navigates and validates downstream).
    /// `None` when nothing applies.
    pub fn chosen_input(&self) -> Option<String> {
        let visible = self.visible_candidates();
        if let Some(c) = visible.get(self.picker.selected) {
            return Some(c.path.to_string_lossy().into_owned());
        }
        let q = self.picker.query().trim();
        if !q.is_empty() {
            return Some(q.to_string());
        }
        None
    }
}

/// `true` when `s` starts with a Windows drive prefix like `C:`.
/// Used only under `cfg!(windows)` to route native absolute paths into path mode.
fn has_windows_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b, [drive, b':', ..] if drive.is_ascii_alphabetic())
}

/// Resolve a path-prefix ending in a separator (e.g. `~/src/`, `/etc/`, `../`, or on Windows `C:\Users\`) to an absolute directory.
/// `~` expands to home; relative prefixes join `base`.
fn resolve_dir_prefix(prefix: &str, base: &Path) -> PathBuf {
    // Strip trailing separators; `/` always, `\` additionally on Windows
    let trimmed = if cfg!(windows) {
        prefix.trim_end_matches(['/', '\\'])
    } else {
        prefix.trim_end_matches('/')
    };
    if prefix.starts_with('/') {
        if trimmed.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(trimmed)
        }
    } else if cfg!(windows) && Path::new(prefix).is_absolute() {
        // Windows drive-absolute (`C:\…`) or UNC
        // Keep the root separator for a bare drive (`C:\` trims to `C:`, which is drive-RELATIVE without it)
        if has_windows_drive_prefix(trimmed) && trimmed.len() == 2 {
            PathBuf::from(format!("{trimmed}\\"))
        } else {
            PathBuf::from(trimmed)
        }
    } else if trimmed == "~" || prefix.starts_with("~/") {
        let rest = trimmed.trim_start_matches('~').trim_start_matches('/');
        let mut home = distill_dirs::home_dir().unwrap_or_else(|| base.to_path_buf());
        if !rest.is_empty() {
            home.push(rest);
        }
        home
    } else if trimmed.is_empty() {
        base.to_path_buf()
    } else {
        base.join(trimmed)
    }
}

/// List the immediate subdirectories of `parent` as location candidates, sorted case-insensitively
/// by name and capped at [`LOCATION_DIR_LISTING_CAP`].
fn read_subdirs(
    parent: &Path,
    worktrees: &std::collections::HashMap<PathBuf, String>,
) -> Vec<LocationCandidate> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<LocationCandidate> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // `path.is_dir()` follows symlinks so symlinked dirs are offered.
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Match the index's canonical keys; resolve symlinks on the entry itself (a lexical `parent_canon.join(name)` would not)
            let worktree = if worktrees.is_empty() {
                None
            } else {
                let canon = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
                worktrees.get(&canon).cloned()
            };
            out.push(LocationCandidate {
                label: name,
                detail: crate::recent_dirs::display_path(&path),
                path,
                worktree,
            });
            if out.len() >= LOCATION_DIR_LISTING_CAP {
                break;
            }
        }
    }
    out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    out
}

/// Per-call config for the location picker's shared-picker input handler.
/// Search is always active (no `/` to start), search is not disabled, and rows are flat (no expand / tabs / filter / action keys).
fn location_picker_config<'a>() -> crate::views::picker::PickerConfig<'a> {
    crate::views::picker::PickerConfig {
        title: None,
        show_search_hint: false,
        expandable: false,
        esc_clears_query: false,
        shortcuts: None,
        pending_hint: None,
        shortcuts_area: None,
        non_selectable: &[],
        non_selectable_clickable: &[],
        tabs: None,
        active_tab: 0,
        filter_label: None,
        filter_key_hint: None,
        filter_active: false,
        header_note: None,
        action_keys: &[],
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: crate::appearance::cache::load_vim_mode(),
    }
}

/// Resolve session ids to live `DashboardRowId`s and back. Pin/reorder persistence keys are stable
/// session ids (not per-process `AgentId`), so a restart resolves them against the new process's
/// agents instead of attaching to whatever happens to have the same `AgentId(usize)`.
pub struct SessionIdResolver {
    /// Maps session_id to AgentId (top-level).
    top: std::collections::HashMap<String, crate::app::agent::AgentId>,
    /// Maps agent_session_id to a set of (child_session_id, AgentId-of-parent).
    subs: std::collections::HashMap<
        String,
        std::collections::HashMap<String, crate::app::agent::AgentId>,
    >,
    /// Reverse: maps AgentId to session_id (for `to_persisted`).
    top_rev: std::collections::HashMap<crate::app::agent::AgentId, String>,
    workspace: std::collections::HashSet<String>,
}

impl SessionIdResolver {
    /// Resolve queries for a collided id therefore return the first-seen `AgentId`, which is
    /// deterministic given the `IndexMap` iteration order.
    pub fn from_agents(
        agents: &indexmap::IndexMap<crate::app::agent::AgentId, crate::app::agent_view::AgentView>,
    ) -> Self {
        Self::from_agents_and_workspace(agents, None)
    }

    pub(crate) fn from_agents_and_workspace(
        agents: &indexmap::IndexMap<crate::app::agent::AgentId, crate::app::agent_view::AgentView>,
        workspace: Option<&crate::app::workspace_layout::WorkspaceView>,
    ) -> Self {
        let mut top = std::collections::HashMap::new();
        let mut top_rev = std::collections::HashMap::new();
        let mut subs: std::collections::HashMap<
            String,
            std::collections::HashMap<String, crate::app::agent::AgentId>,
        > = std::collections::HashMap::new();
        for (id, agent) in agents {
            if let Some(sid) = agent.session.session_id.as_ref() {
                let sid_str = sid.0.to_string();
                if let Some(prev) = top.get(&sid_str) {
                    // Top-level collision: keep first mapping.
                    // And call out the subagents we're dropping by name so a debugger can correlate the missing children back to the losing parent
                    let dropped: Vec<&str> =
                        agent.subagent_sessions.keys().map(String::as_str).collect();
                    tracing::warn!(
                        session_id = %sid_str,
                        first = ?prev,
                        duplicate = ?id,
                        dropped_subagents = ?dropped,
                        "SessionIdResolver: duplicate session_id; keeping first mapping (subagents of losing parent are dropped)"
                    );
                } else {
                    top.insert(sid_str.clone(), *id);
                    top_rev.insert(*id, sid_str.clone());
                    // Populate the subs map via the entry()/or_insert_with() API so the "first wins" rule is explicit
                    // Defensive: a colliding child_session_id within a single parent's map would also warn rather than silently overwrite
                    let children = subs.entry(sid_str.clone()).or_default();
                    for child_sid in agent.subagent_sessions.keys() {
                        if let Some(prev_child) = children.get(child_sid) {
                            tracing::warn!(
                                parent_session_id = %sid_str,
                                child_session_id = %child_sid,
                                first = ?prev_child,
                                duplicate = ?id,
                                "SessionIdResolver: duplicate child session_id under same parent; keeping first mapping"
                            );
                        } else {
                            children.insert(child_sid.clone(), *id);
                        }
                    }
                    if children.is_empty() {
                        subs.remove(&sid_str);
                    }
                }
            }
        }
        let workspace = workspace
            .into_iter()
            .flat_map(|workspace| workspace.members.iter())
            .filter(|member| matches!(member.kind, distill_dashboard_store::MemberKind::Build))
            .map(|member| member.session_id.to_string())
            .filter(|session_id| !top.contains_key(session_id))
            .collect();
        Self {
            top,
            subs,
            top_rev,
            workspace,
        }
    }

    /// Empty resolver: used when no agents have been loaded yet.
    /// Resolves nothing, persists nothing.
    pub fn empty() -> Self {
        Self {
            top: std::collections::HashMap::new(),
            subs: std::collections::HashMap::new(),
            top_rev: std::collections::HashMap::new(),
            workspace: std::collections::HashSet::new(),
        }
    }

    pub fn resolve(&self, pid: &PersistedRowId) -> Option<DashboardRowId> {
        match pid {
            PersistedRowId::TopLevel { session_id } => self
                .top
                .get(session_id)
                .copied()
                .map(DashboardRowId::TopLevel)
                .or_else(|| {
                    self.workspace
                        .contains(session_id)
                        .then(|| DashboardRowId::Workspace {
                            session_id: session_id.clone(),
                        })
                }),
            PersistedRowId::Subagent {
                parent_session_id,
                child_session_id,
            } => self
                .subs
                .get(parent_session_id)
                .and_then(|m| m.get(child_session_id).copied())
                .map(|parent| DashboardRowId::Subagent {
                    parent,
                    child_session_id: child_session_id.clone(),
                }),
        }
    }

    pub fn to_persisted(&self, id: &DashboardRowId) -> Option<PersistedRowId> {
        match id {
            DashboardRowId::TopLevel(agent_id) => {
                self.top_rev
                    .get(agent_id)
                    .map(|sid| PersistedRowId::TopLevel {
                        session_id: sid.clone(),
                    })
            }
            DashboardRowId::Subagent {
                parent,
                child_session_id,
            } => {
                let parent_sid = self.top_rev.get(parent)?.clone();
                Some(PersistedRowId::Subagent {
                    parent_session_id: parent_sid,
                    child_session_id: child_session_id.clone(),
                })
            }
            DashboardRowId::Workspace { session_id } => Some(PersistedRowId::TopLevel {
                session_id: session_id.clone(),
            }),
            DashboardRowId::Roster { .. } => None,
        }
    }
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardState {
    /// Construct an empty dashboard state.
    /// Persistence is applied via [`Self::apply_persisted`] right after `new()` at the open site.
    pub fn new() -> Self {
        // The dashboard is session-less: its dispatch input offers only pager-global slash commands (hide session-scoped ones such as /compact,
        // /fork, /rewind; they make no sense with no session)
        let mut dispatch = PromptWidget::new();
        dispatch.hide_session_scoped_commands();
        // The peek reply is a quick single-row input; compact mode lowers the `[Pasted: N lines]` chip threshold to 2 lines so multi-line pastes
        // fold instead of overflowing the row
        let mut peek_reply = PromptWidget::new();
        peek_reply.set_compact(true);
        Self {
            selected: None,
            hovered_row: None,
            selected_section: None,
            hovered_section: None,
            // The "Inactive" section (roster-only idle/dormant sessions owned by OTHER pager processes; see
            // `RowState::Inactive`) is background noise relative to the sessions you're actively cycling
            // between.
            collapsed_sections: std::iter::once(SectionKey::State(RowState::Inactive)).collect(),
            selected_idle_overflow: false,
            hovered_idle_overflow: false,
            idle_show_all: false,
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
            grouping: Grouping::default(),
            workspace_grouping: None,
            filter: Filter::None,
            search_mode: false,
            dispatch,
            pending_effects: Vec::new(),
            paste_probe_in_flight: 0,
            deferred_dispatch_send: None,
            deferred_peek_send: None,
            last_send_at: None,
            peek: None,
            peek_viewport: None,
            peek_reply,
            peek_reply_rect: None,
            peek_reply_cwd: None,
            peek_reply_target_cwd: None,
            rename: None,
            error_toast: None,
            delete_confirm: None,
            workspace_membership_mode: false,
            selected_stop_action: None,
            spinner_tick: 0,
            painted_animations: PaintedAnimations::default(),
            row_rects: Vec::new(),
            row_delete_rects: Vec::new(),
            hovered_delete: None,
            conversation_row_ids: std::collections::HashSet::new(),
            section_rects: Vec::new(),
            idle_overflow_rect: None,
            last_area: Rect::default(),
            attached_agent: None,
            popup_close_rect: None,
            popup_outer_rect: None,
            new_agent_button_hit: crate::app::agent_view::HitArea::default(),
            open_session_button_hit: crate::app::agent_view::HitArea::default(),
            worktree_toggle_hit: crate::app::agent_view::HitArea::default(),
            slash_dropdown_items_area: None,
            slash_dropdown_hit: Default::default(),
            file_search_dropdown_items_area: None,
            list_focused: false,
            last_mouse_pos: None,
            last_click: None,
            last_prompt_click: None,
            peek_close_rect: None,
            dispatch_rect: None,
            viewport_offset: 0,
            manual_scroll_active: false,
            shortcuts_modal: None,
            pending_model: None,
            pending_mode: DashboardDispatchMode::Normal,
            models: crate::acp::model_state::ModelState::default(),
            location_picker: None,
            location_hit: crate::app::agent_view::HitArea::default(),
            upgrade_cta_hit: crate::app::agent_view::HitArea::default(),
            pinned_upgrade_cta_live: false,
            dispatch_worktree: false,
            cwd_has_git_ancestor: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            worktree_dialog: None,
            pending_worktree_prompt: None,
            pending_worktree_attach: false,
            voice_listening: false,
            voice_interim: None,
            multiline_mode: false,
            usage_modal: None,
            // Fresh dashboard with no rows seeded, so the `+ New Agent` button is the default cursor target
            // Open sites that want a specific row seeded call `focus_row` after construction, which clears this atomically
            actions_focus: Some(ActionsFocus::NewAgent),
        }
    }

    /// Adopt the shared slash MRU store (owned by `AppView`) into both the dispatch input and the peek-reply input so dashboard slash completion
    /// shares command recency with agent prompts.
    pub(crate) fn adopt_slash_mru(
        &mut self,
        mru: std::rc::Rc<std::cell::RefCell<crate::slash::mru::SlashMru>>,
    ) {
        self.dispatch.adopt_slash_mru(mru.clone());
        self.peek_reply.adopt_slash_mru(mru);
    }

    /// Adopt the shared per-command tag map (owned by `AppView`) into both the dispatch input and the peek-reply input so dashboard slash completion
    /// renders the same tags as agent prompts.
    pub(crate) fn adopt_command_tags(
        &mut self,
        command_tags: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, String>>>,
    ) {
        self.dispatch.adopt_command_tags(command_tags.clone());
        self.peek_reply.adopt_command_tags(command_tags);
    }

    pub(crate) fn set_screen_mode(&mut self, mode: crate::app::ScreenMode) {
        self.dispatch.set_screen_mode(mode);
        self.peek_reply.set_screen_mode(mode);
    }

    pub(crate) fn set_recap_visible(&mut self, visible: bool) {
        self.dispatch.set_recap_visible(visible);
        self.peek_reply.set_recap_visible(visible);
    }

    pub(crate) fn set_voice_visible(&mut self, visible: bool) {
        self.dispatch.set_voice_visible(visible);
        self.peek_reply.set_voice_visible(visible);
    }

    /// Gate `/auto` on both dashboard prompt registries (dispatch and peek reply).
    /// See [`crate::slash::SlashController::set_auto_mode_available`].
    pub(crate) fn set_auto_mode_available(&mut self, available: bool) {
        self.dispatch.set_auto_mode_available(available);
        self.peek_reply.set_auto_mode_available(available);
    }

    /// Replace the restricted slash-command deny list on both dashboard prompt registries (dispatch and peek reply).
    pub(crate) fn set_restricted_commands(&mut self, names: &[String]) {
        self.dispatch.set_restricted_commands(names);
        self.peek_reply.set_restricted_commands(names);
    }

    /// Prefix error text that has no glyph of its own. Messages that already
    /// carry a glyph should not go through this helper.
    pub(crate) fn set_error_toast(&mut self, msg: &str) {
        self.error_toast = Some(format!("{} {msg}", crate::glyphs::ballot_x()));
    }

    /// Forget header and actions-row click rects from the previous frame.
    /// Called once per frame before any renderer, so a rect never outlives its paint.
    /// `set(None)` keeps mouse-driven `hovered` flags.
    pub fn clear_chrome_hit_areas(&mut self) {
        self.location_hit.set(None);
        self.upgrade_cta_hit.set(None);
        self.new_agent_button_hit.set(None);
        self.open_session_button_hit.set(None);
        self.worktree_toggle_hit.set(None);
    }

    /// True when the actions row's `+ New Agent` button holds the cursor.
    pub fn new_agent_button_focused(&self) -> bool {
        self.actions_focus == Some(ActionsFocus::NewAgent)
    }

    /// True when the actions row's `Open Previous` button holds the cursor.
    pub fn open_session_button_focused(&self) -> bool {
        self.actions_focus == Some(ActionsFocus::OpenPrevious)
    }

    /// True when the actions row's `Worktree` toggle holds the cursor.
    pub fn worktree_toggle_focused(&self) -> bool {
        self.actions_focus == Some(ActionsFocus::Worktree)
    }

    /// Move the cursor onto an actions-row item.
    /// Clears any row/section selection so the "actions row focused means no row selected" invariant stays honoured.
    /// Idempotent; safe to call when the item is already focused.
    pub fn focus_action(&mut self, item: ActionsFocus) {
        self.actions_focus = Some(item);
        self.selected = None;
        self.selected_section = None;
        self.selected_idle_overflow = false;
        self.delete_confirm = None;
    }

    /// Focus the actions row's `+ New Agent` button.
    pub fn focus_new_agent_button(&mut self) {
        self.focus_action(ActionsFocus::NewAgent);
    }

    pub fn focus_open_session_button(&mut self) {
        self.focus_action(ActionsFocus::OpenPrevious);
    }

    /// The painted actions-row item one step left or right of the focused one; `None` at either end (no wrap) or when the actions
    /// row has no focus. See [`ActionsFocus::neighbour`].
    fn actions_neighbour(&self, step: Step) -> Option<ActionsFocus> {
        self.actions_focus?.neighbour(self, step)
    }

    /// What a click (or Enter) on the focused actions-row item does; `None` when no item holds the cursor.
    /// `+ New Agent` with a typed draft is the caller's business: the list-focused Enter path sends the draft instead.
    pub(crate) fn focused_action_click(&self) -> Option<Action> {
        Some(match self.actions_focus? {
            ActionsFocus::NewAgent => Action::DashboardCreateNewAgentWithDetail,
            ActionsFocus::OpenPrevious => Action::ShowSessionPicker,
            ActionsFocus::Worktree => Action::DashboardToggleWorktree,
        })
    }

    /// The footer label for Enter on the focused actions-row item.
    /// `+ New Agent` with a typed draft sends it rather than creating an empty session, and the label says so.
    pub(crate) fn focused_action_label(&self) -> Option<&'static str> {
        Some(match self.actions_focus? {
            ActionsFocus::NewAgent if self.focused_new_agent_sends_draft() => "send",
            ActionsFocus::NewAgent => "create",
            ActionsFocus::OpenPrevious => "open previous",
            ActionsFocus::Worktree if self.worktree_armed() => "disable worktree",
            ActionsFocus::Worktree => "enable worktree",
        })
    }

    /// True when Enter on the focused `+ New Agent` would send the typed draft instead of creating an empty session.
    pub(crate) fn focused_new_agent_sends_draft(&self) -> bool {
        self.new_agent_button_focused() && !self.dispatch.text().trim().is_empty()
    }

    /// Search mode treats an empty buffer as a filter query, so these keys stay with the caret.
    fn list_keys_active(&self) -> bool {
        self.list_focused || (self.dispatch.text().is_empty() && !self.search_mode)
    }

    /// Whether the next dispatch goes into a fresh git worktree: the mode is on and the cwd is a git repo, so it can take effect.
    /// The actions row's labels and the footer's Enter hint both read this so they cannot drift apart.
    pub(crate) fn worktree_armed(&self) -> bool {
        self.dispatch_worktree && self.cwd_has_git_ancestor
    }

    /// Focus the row identified by `id`. Clears the actions-row cursor so the cursor states stay
    /// mutually exclusive. Any caller that mutates `selected` directly bypasses this helper
    /// at its own risk; the invariant only holds when both fields are written through here.
    pub fn focus_row(&mut self, id: DashboardRowId) {
        if self
            .delete_confirm
            .as_ref()
            .is_some_and(|(armed, _)| armed != &id)
        {
            self.delete_confirm = None;
        }
        self.selected = Some(id);
        self.actions_focus = None;
        self.selected_section = None;
        self.selected_idle_overflow = false;
    }

    /// Follow session overlay attach from `previous` to `new_id` when attach already names `previous`.
    /// No-op otherwise so overlay chrome is never invented.
    /// Keeps row focus aligned with attach.
    pub fn repoint_attach_if_on(&mut self, previous: AgentId, new_id: AgentId) {
        if self.attached_agent == Some(previous) {
            self.attached_agent = Some(new_id);
            self.focus_row(DashboardRowId::TopLevel(new_id));
        }
    }

    /// Focus the section header identified by `key`; the third cursor target alongside rows and the `+ New Agent` button.
    /// Clears the row selection and button focus so exactly one cursor is active.
    pub fn focus_section(&mut self, key: SectionKey) {
        self.selected_section = Some(key);
        self.selected = None;
        self.actions_focus = None;
        self.selected_idle_overflow = false;
        self.delete_confirm = None;
    }

    /// Focus the Idle group's "N more" overflow toggle; the fourth cursor target.
    /// Clears the other three so exactly one cursor is active.
    pub fn focus_idle_overflow(&mut self) {
        self.selected_idle_overflow = true;
        self.selected = None;
        self.selected_section = None;
        self.actions_focus = None;
        self.delete_confirm = None;
    }

    pub(in crate::views::dashboard) fn set_list_focused(&mut self, focused: bool) {
        self.list_focused = focused;
        if !focused {
            self.delete_confirm = None;
        }
    }

    /// The armed row while its [`CONFIRM_WINDOW`] is still live, clearing an expired arm as a side effect.
    /// The accessor the dispatcher and mouse handler share so "armed on screen" and "armed for delete" never diverge.
    pub fn armed_delete_row(&mut self) -> Option<DashboardRowId> {
        match &self.delete_confirm {
            Some((id, at)) if at.elapsed() < CONFIRM_WINDOW => Some(id.clone()),
            Some(_) => {
                self.delete_confirm = None;
                None
            }
            None => None,
        }
    }

    /// Read-only counterpart of [`Self::armed_delete_row`] for the renderer (does not clear an expired arm).
    pub fn armed_delete_row_ref(&self) -> Option<&DashboardRowId> {
        self.delete_confirm
            .as_ref()
            .filter(|(_, at)| at.elapsed() < CONFIRM_WINDOW)
            .map(|(id, _)| id)
    }

    pub fn arm_delete(&mut self, id: DashboardRowId) {
        self.delete_confirm = Some((id, Instant::now()));
    }

    /// Whether `id` is a chat-conversation roster row, which the dashboard can't delete yet (see [`Self::conversation_row_ids`]).
    pub fn row_is_conversation(&self, id: &DashboardRowId) -> bool {
        matches!(id, DashboardRowId::Roster { session_id }
            if self.conversation_row_ids.contains(session_id))
    }

    /// Enforce the invariant that a delete arm belongs to the selected row.
    /// Selection changes routed through the focus helpers already disarm, but `reanchor_selection` / `gc_stale_refs` can drop or move `selected`
    /// directly; without this a stale arm would let a later `y` delete a row that is no longer selected.
    fn sync_delete_confirm_to_selection(&mut self) {
        if let Some((armed, _)) = self.delete_confirm.as_ref()
            && self.selected.as_ref() != Some(armed)
        {
            self.delete_confirm = None;
        }
    }

    /// Toggle whether the Idle group shows every agent (`true`) or caps to the recent ones with the rest folded behind the overflow row (`false`).
    /// In-memory only; resets to capped on the next pager start.
    /// Mirrors the in-memory lifetime of section collapse.
    pub fn toggle_idle_show_all(&mut self) {
        self.idle_show_all = !self.idle_show_all;
    }

    /// Whether `key`'s section is currently collapsed (its rows hidden).
    pub fn is_section_collapsed(&self, key: SectionKey) -> bool {
        self.collapsed_sections.contains(&key)
    }

    /// Collapse (`true`) or expand (`false`) the section identified by `key`.
    /// Idempotent.
    /// Shared by the keyboard (Left/Right on a selected header) and the mouse (click on a header).
    pub fn set_section_collapsed(&mut self, key: SectionKey, collapsed: bool) {
        if collapsed {
            self.collapsed_sections.insert(key);
        } else {
            self.collapsed_sections.remove(&key);
        }
    }

    /// Toggle the collapsed state of the section identified by `key`.
    pub fn toggle_section(&mut self, key: SectionKey) {
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    /// Construct from persisted state, resolving session-id keys to live `DashboardRowId`s via the given resolver.
    /// Unresolvable ids (sessions that have closed, or never existed) are dropped.
    pub fn from_persisted(p: &PersistedDashboard, resolver: &SessionIdResolver) -> Self {
        let mut s = Self::new();
        s.apply_persisted(p, resolver);
        s
    }

    pub(crate) fn apply_persisted(&mut self, p: &PersistedDashboard, resolver: &SessionIdResolver) {
        self.grouping = p.grouping;
        self.pinned = p
            .pinned
            .iter()
            .filter_map(|pid| resolver.resolve(pid))
            .collect();
        self.reorder = p
            .reorder
            .iter()
            .filter_map(|pid| resolver.resolve(pid))
            .collect();
    }

    /// Snapshot back to a persistable form.
    /// Top-level rows whose underlying agent does not yet have a `session_id` are dropped (they can't be persisted yet; but they'll remain in the
    /// in-memory state for the rest of this process lifetime).
    pub fn to_persisted(&self, enabled: bool, resolver: &SessionIdResolver) -> PersistedDashboard {
        PersistedDashboard {
            enabled,
            grouping: self.grouping,
            pinned: self
                .pinned
                .iter()
                .filter_map(|id| resolver.to_persisted(id))
                .collect(),
            reorder: self
                .reorder
                .iter()
                .filter_map(|id| resolver.to_persisted(id))
                .collect(),
        }
    }

    pub fn rebind_workspace_identities(
        &mut self,
        old: &SessionIdResolver,
        new: &SessionIdResolver,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let rebind = |id: &DashboardRowId| match old.to_persisted(id) {
            Some(persisted) => new.resolve(&persisted),
            None => Some(id.clone()),
        };

        if let Some(selected) = self.selected.as_ref()
            && let Some(rebound) = rebind(selected)
        {
            self.selected = Some(rebound);
        }
        self.hovered_row = self.hovered_row.as_ref().and_then(&rebind);
        self.hovered_delete = self.hovered_delete.as_ref().and_then(&rebind);
        self.last_click = self
            .last_click
            .take()
            .and_then(|(row, at)| rebind(&row).map(|row| (row, at)));
        self.delete_confirm = self
            .delete_confirm
            .take()
            .and_then(|(row, at)| rebind(&row).map(|row| (row, at)));

        if let Some(rename) = self.rename.as_mut() {
            match rebind(&rename.row) {
                Some(row) if !row.is_workspace() => rename.row = row,
                _ => self.rename = None,
            }
        }
        let peek_row = self.peek.as_ref().map(|peek| peek.row.clone());
        if let Some(peek_row) = peek_row {
            match rebind(&peek_row) {
                Some(row) if !row.is_workspace() => {
                    if let Some(peek) = self.peek.as_mut() {
                        peek.row = row.clone();
                    }
                    if let Some(lease) = self.peek_viewport.as_mut() {
                        lease.row = row;
                    }
                }
                _ => {
                    self.restore_peek_viewport(agents);
                    self.set_peek(None);
                }
            }
        }
    }

    pub fn prepare_agent_unbind(
        &mut self,
        agent_ids: &std::collections::HashSet<AgentId>,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let targets_agent = |row: &DashboardRowId| {
            agent_ids
                .iter()
                .any(|agent_id| row.is_owned_by_agent(*agent_id))
        };
        if self
            .rename
            .as_ref()
            .is_some_and(|rename| targets_agent(&rename.row))
        {
            self.rename = None;
        }
        if self
            .peek
            .as_ref()
            .is_some_and(|peek| targets_agent(&peek.row))
            || self
                .peek_viewport
                .as_ref()
                .is_some_and(|lease| targets_agent(&lease.row))
        {
            self.restore_peek_viewport(agents);
            self.set_peek(None);
        }
    }

    pub fn reconcile_visible_rows(
        &mut self,
        before: &[Focusable],
        after: &[Focusable],
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let after_rows = after
            .iter()
            .filter_map(|focusable| match focusable {
                Focusable::Row(id) => Some(id.clone()),
                Focusable::Section(_) | Focusable::IdleOverflow => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let row_survives = |row: &DashboardRowId| after_rows.contains(row);

        if self
            .rename
            .as_ref()
            .is_some_and(|rename| !row_survives(&rename.row))
        {
            self.rename = None;
        }
        if self
            .peek
            .as_ref()
            .is_some_and(|peek| !row_survives(&peek.row))
        {
            self.restore_peek_viewport(agents);
            self.set_peek(None);
        }
        if self
            .hovered_row
            .as_ref()
            .is_some_and(|row| !row_survives(row))
        {
            self.hovered_row = None;
        }
        if self
            .hovered_delete
            .as_ref()
            .is_some_and(|row| !row_survives(row))
        {
            self.hovered_delete = None;
        }
        if self
            .last_click
            .as_ref()
            .is_some_and(|(row, _)| !row_survives(row))
        {
            self.last_click = None;
        }
        if self
            .delete_confirm
            .as_ref()
            .is_some_and(|(row, _)| !row_survives(row))
        {
            self.delete_confirm = None;
        }

        let Some(selected) = self.selected.clone() else {
            return;
        };
        if row_survives(&selected) {
            return;
        }
        let replacement = before
            .iter()
            .position(|focusable| matches!(focusable, Focusable::Row(id) if id == &selected))
            .and_then(|index| {
                before
                    .get(index + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|focusable| match focusable {
                        Focusable::Row(id) if row_survives(id) => Some(id.clone()),
                        Focusable::Row(_) | Focusable::Section(_) | Focusable::IdleOverflow => None,
                    })
                    .next()
                    .or_else(|| {
                        before
                            .get(..index)
                            .unwrap_or(&[])
                            .iter()
                            .rev()
                            .find_map(|focusable| match focusable {
                                Focusable::Row(id) if row_survives(id) => Some(id.clone()),
                                Focusable::Row(_)
                                | Focusable::Section(_)
                                | Focusable::IdleOverflow => None,
                            })
                    })
            });
        match replacement {
            Some(row) => self.focus_row(row),
            None => self.focus_new_agent_button(),
        }
        self.clear_manual_scroll();
    }

    /// Garbage-collect stale row ids from `pinned` / `reorder`. Called on dashboard open: any persisted
    /// id whose underlying agent/subagent no longer exists is silently dropped (e.g. a pinned row whose
    /// agent was deleted).
    pub fn gc_stale_refs(&mut self, alive: &dyn Fn(&DashboardRowId) -> bool) {
        self.pinned.retain(|id| alive(id));
        self.reorder.retain(|id| alive(id));
        if let Some(rn) = self.rename.as_ref()
            && !alive(&rn.row)
        {
            self.rename = None;
        }
        if let Some(sel) = self.selected.as_ref()
            && !alive(sel)
        {
            self.selected = None;
        }
        if let Some(p) = self.peek.as_ref()
            && !alive(&p.row)
        {
            self.set_peek(None);
        }
        if let Some(h) = self.hovered_row.as_ref()
            && !alive(h)
        {
            self.hovered_row = None;
        }
        if let Some((id, _)) = self.last_click.as_ref()
            && !alive(id)
        {
            self.last_click = None;
        }
        // `attached_agent` is keyed by `AgentId`, not `DashboardRowId`, so we
        // build the equivalent `DashboardRowId::TopLevel` and probe `alive`
        // Clears the popup gracefully when the underlying session was closed outside the dashboard (e.g. via another surface).
        if let Some(agent_id) = self.attached_agent
            && !alive(&DashboardRowId::TopLevel(agent_id))
        {
            // close_popup() also clears `popup_close_rect` / `popup_outer_rect` so the invariant
            // (a `None` attached_agent means `None` hit rects) holds at every close site, not just here
            self.close_popup();
        }
        self.sync_delete_confirm_to_selection();
    }

    /// Switch grouping (`Ctrl+G`).
    pub fn toggle_grouping(&mut self) {
        self.set_grouping(self.grouping.toggled());
    }

    pub(crate) fn set_grouping(&mut self, grouping: Grouping) {
        if self.grouping == grouping {
            return;
        }
        self.grouping = grouping;
        self.repair_grouping_change(grouping);
    }

    pub(crate) fn observe_workspace_grouping(&mut self, grouping: Grouping) {
        if self.workspace_grouping == Some(grouping) {
            return;
        }
        self.workspace_grouping = Some(grouping);
        self.repair_grouping_change(grouping);
    }

    pub(crate) fn clear_workspace_grouping(&mut self) {
        self.workspace_grouping = None;
    }

    fn repair_grouping_change(&mut self, grouping: Grouping) {
        // Section-header cursors are invalid outside State grouping.
        if self.selected_section.is_some() && !matches!(grouping, Grouping::State) {
            self.focus_new_agent_button();
        }
        // Re-engage selection follow; a grouping switch reshuffles the visible row order, so the user's prior manual-scroll position no longer
        // points at the same content
        self.clear_manual_scroll();
    }

    /// Toggle pin on the selected row.
    /// Returns the toggled id, or `None` if no row is selected.
    pub fn toggle_pin_selected(&mut self) -> Option<DashboardRowId> {
        let id = self.selected.clone()?;
        if !self.pinned.remove(&id) {
            self.pinned.insert(id.clone());
        }
        Some(id)
    }

    /// Forward a scroll event from the app-level mouse pipeline. Mouse wheel is decoupled from the
    /// selected row: scrolling only moves the viewport, leaving `selected` alone.
    pub fn handle_scroll(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        self.manual_scroll_active = true;
        if lines > 0 {
            self.viewport_offset = self.viewport_offset.saturating_add(lines as usize);
        } else {
            self.viewport_offset = self.viewport_offset.saturating_sub((-lines) as usize);
        }
    }

    /// Re-engage selection-driven viewport tracking. Called by any path that owns selection (arrow
    /// keys, dispatcher-side `DashboardSelect*`, row click, filter / grouping change) so the next
    /// render snaps the viewport back to the selected row. No-op when the flag is already clear.
    pub fn clear_manual_scroll(&mut self) {
        self.manual_scroll_active = false;
    }

    /// Clamp `viewport_offset` so that `selected_line_idx` (if any) stays within the visible
    /// `viewport_h` window. The renderer is factored for testability; it is not strictly read-only.
    pub fn clamp_viewport(
        &mut self,
        selected_line: Option<usize>,
        viewport_h: usize,
        total_lines: usize,
    ) -> usize {
        let mut offset = self.viewport_offset;
        if !self.manual_scroll_active
            && let Some(sel_idx) = selected_line
            && viewport_h > 0
        {
            if sel_idx < offset {
                offset = sel_idx;
            } else if sel_idx >= offset + viewport_h {
                offset = sel_idx + 1 - viewport_h;
            }
        }
        let max_offset = total_lines.saturating_sub(viewport_h);
        if offset > max_offset {
            offset = max_offset;
        }
        self.viewport_offset = offset;
        offset
    }

    /// Set the peek panel, enforcing the "`peek_close_rect` is None whenever `peek` is None" invariant.
    /// Every site that toggles `peek` now goes through this helper so the close-rect can't be left
    /// stale.
    pub fn set_peek(&mut self, peek: Option<PeekPanelState>) {
        if peek.is_none() {
            self.peek_close_rect = None;
            self.peek_reply_rect = None;
        }
        let row_changed = peek.as_ref().map(|p| &p.row) != self.peek.as_ref().map(|p| &p.row);
        if row_changed {
            self.clear_peek_reply();
            self.deferred_peek_send = None;
        }
        self.peek = peek;
    }

    pub fn restore_peek_viewport(
        &mut self,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let Some(lease) = self.peek_viewport.take() else {
            return;
        };
        let Some(sb) = scrollback_mut_for_row(&lease.row, agents) else {
            return;
        };
        let page_flip = lease.page_flip_entry;
        let w = lease.snapshot.last_width;
        let h = lease.snapshot.viewport_height;
        sb.restore_viewport_snapshot(lease.snapshot);
        if let Some(entry_id) = page_flip
            && let Some(idx) = sb.index_of_id(entry_id)
        {
            if w > 0 && h > 0 {
                sb.prepare_layout(w, h);
            }
            sb.set_selected(Some(idx));
            sb.page_flip_to_entry(idx);
        }
    }

    pub fn begin_peek_viewport(
        &mut self,
        row: DashboardRowId,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        if self
            .peek_viewport
            .as_ref()
            .is_some_and(|lease| lease.row == row)
        {
            return;
        }
        self.restore_peek_viewport(agents);
        let Some(sb) = scrollback_mut_for_row(&row, agents) else {
            return;
        };
        let snapshot = sb.capture_viewport_snapshot();
        sb.set_view_mode(crate::scrollback::state::ViewMode::AllTurns);
        sb.enable_follow_mode();
        self.peek_viewport = Some(PeekViewportLease {
            row,
            snapshot,
            page_flip_entry: None,
        });
    }

    pub(crate) fn note_page_flip_for_lease(
        &mut self,
        agent_id: AgentId,
        entry_id: crate::scrollback::EntryId,
        agents: &indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let Some(lease) = self.peek_viewport.as_mut() else {
            return;
        };
        if !lease.row.matches_top_level_agent(agent_id) {
            return;
        }
        let Some(sb) = agents.get(&agent_id).map(|agent| &agent.scrollback) else {
            return;
        };
        if sb.index_of_id(entry_id).is_none() {
            return;
        }
        if !sb.is_follow_preserve_scroll() {
            return;
        }
        lease.page_flip_entry = Some(entry_id);
    }

    /// Clear the peek reply draft AND its undo history.
    pub(crate) fn clear_peek_reply(&mut self) {
        self.peek_reply.set_text("");
        self.peek_reply.clear_history();
        self.last_prompt_click = None;
    }

    /// Record the peeked agent's working directory so the reply's `@` picker can later resolve `@paths` against it.
    /// Called by the render pass (the only place with the agents map); the actual `retarget` is deferred to [`Self::ensure_peek_reply_cwd`].
    pub(crate) fn set_peek_reply_target_cwd(&mut self, cwd: Option<PathBuf>) {
        self.peek_reply_target_cwd = cwd;
    }

    /// Applied only when it differs from the daemon's current root, and only when the user composes
    /// into the reply (never on a bare cursor move), because `retarget` throws away the built matcher
    /// daemon and the next @-use rebuilds it.
    fn ensure_peek_reply_cwd(&mut self) {
        if let Some(target) = self.peek_reply_target_cwd.clone()
            && self.peek_reply_cwd.as_deref() != Some(target.as_path())
        {
            self.peek_reply.file_search.retarget(&target);
            self.peek_reply_cwd = Some(target);
        }
    }

    /// The file-search state backing the `@` dropdown that is actually on screen: the peek reply's while the panel is on screen (the dropdown is drawn
    /// from `peek_reply` then; see `render_dashboard`), otherwise the dispatch box's.
    /// Used to route mouse-wheel scrolling to the SAME picker the user is looking at, so wheel navigation matches the rendered list.
    pub(crate) fn dropdown_file_search_mut(
        &mut self,
    ) -> &mut crate::views::file_search::FileSearchState {
        if self.peek_owns_input() {
            &mut self.peek_reply.file_search
        } else {
            &mut self.dispatch.file_search
        }
    }

    /// Feed a key to the reply widget after ensuring its `@` picker is rooted at the peeked agent's cwd.
    /// All reply-composing paths route through here so the lazy retarget lands before a
    /// freshly typed `@` kicks off the directory walk on a stale root.
    fn peek_reply_handle_key(&mut self, key: &KeyEvent) -> PromptEvent {
        self.ensure_peek_reply_cwd();
        self.peek_reply.handle_key(key)
    }

    pub fn close_peek(&mut self) {
        self.set_peek(None);
    }

    /// Atomically clear all popup state when the overlay closes. Centralising the clear here keeps the
    /// invariant honest at every close site (Esc / Ctrl+\\ key handlers, `[✗]` mouse click,
    /// `dispatch_exit_dashboard`, `dispatch_open_dashboard`'s toggle branch).
    pub fn close_popup(&mut self) {
        self.attached_agent = None;
        self.popup_close_rect = None;
        self.popup_outer_rect = None;
    }

    /// Top-level input handler.
    /// Mirrors `AgentView::handle_input`; returns an [`InputOutcome`] for the app to dispatch.
    pub fn handle_input(&mut self, ev: &Event, registry: &ActionRegistry) -> InputOutcome {
        self.handle_input_with_paste_provenance(
            ev,
            registry,
            crate::app::app_view::PasteProvenance::Terminal,
        )
    }

    pub(crate) fn handle_input_with_paste_provenance(
        &mut self,
        ev: &Event,
        registry: &ActionRegistry,
        paste_provenance: crate::app::app_view::PasteProvenance,
    ) -> InputOutcome {
        debug_assert!(
            matches!(ev, Event::Paste(_))
                || paste_provenance == crate::app::app_view::PasteProvenance::Terminal,
            "non-paste dashboard events cannot carry paste provenance"
        );
        // Cheatsheet modal owns the keyboard / mouse when open; its own search input, picker scroll, and chrome buttons would all be inconsistent if
        // the dashboard's row nav got a parallel say
        // Mirrors how `agent_view` short-circuits any `active_modal` before the per-pane handlers run
        if self.shortcuts_modal.is_some() {
            return self.handle_shortcuts_modal_input(ev);
        }

        if self.usage_modal.is_some() {
            return self.handle_usage_modal_input(ev);
        }

        // The location picker owns input while open; its query field, row nav, and chrome buttons would all be inconsistent if the dashboard's own
        // handlers got a parallel say
        if self.location_picker.is_some() {
            return self.handle_location_picker_input(ev);
        }

        // The worktree-label dialog (shown when a dashboard agent is dispatched with worktree mode armed) owns input while open
        if self.worktree_dialog.is_some() {
            return self.handle_worktree_dialog_input(ev);
        }

        // Rename mode owns input until committed or cancelled.
        if let Some(ref mut rn) = self.rename {
            match ev {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    return handle_rename_key(rn, key);
                }
                Event::Paste(text) => return handle_rename_paste(rn, text),
                _ => {}
            }
            return InputOutcome::Unchanged;
        }

        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key, registry),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            // Bracketed paste: wrap magic first (never as text).
            Event::Paste(text) => {
                let peek_owns_paste = self.peek_owns_input();
                if let Some(wrap) =
                    crate::wrap_clipboard_image::try_decode_wrap_host_image_paste(text)
                {
                    return match wrap {
                        crate::wrap_clipboard_image::WrapImagePaste::Image(data) => {
                            let pasted = crate::prompt_images::from_clipboard_data(&data);
                            if peek_owns_paste {
                                if let Some(p) = self.peek.as_mut() {
                                    p.focused = true;
                                }
                                self.ensure_peek_reply_cwd();
                                self.attach_peek_pasted_image(pasted).0
                            } else {
                                self.attach_pasted_image(pasted).0
                            }
                        }
                        crate::wrap_clipboard_image::WrapImagePaste::NoImage => {
                            InputOutcome::Unchanged
                        }
                    };
                }
                self.handle_bracketed_paste(
                    text,
                    peek_owns_paste,
                    paste_provenance.may_probe_clipboard_attachments(),
                )
            }
            _ => InputOutcome::Unchanged,
        }
    }

    /// Bracketed-paste payload into the dispatch input (`peek = false`) or the peek reply (`peek =
    /// true`).
    fn handle_bracketed_paste(
        &mut self,
        text: &str,
        peek: bool,
        probe_clipboard_attachments: bool,
    ) -> InputOutcome {
        // The peek reply has extra preconditions the dispatch input does not.
        let in_question = if peek {
            let Some(p) = self.peek.as_ref() else {
                return InputOutcome::Unchanged;
            };
            // In question mode the `❯ reply` line is hidden; the reply widget only backs the `RejectOnce` / "Other" free-text option
            // Accepting a paste when that option isn't selected would silently fill an
            // invisible buffer that resurfaces if the user later highlights it.
            let in_question = p.question.is_some();
            if in_question {
                let on_reject = p.selected_option.is_some() && p.reject_option == p.selected_option;
                if !on_reject {
                    return InputOutcome::Unchanged;
                }
            }
            // Pasting implies an intent to reply; focus the input and root the `@` picker at the peeked agent's cwd (a pasted `@path` walks it)
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            self.ensure_peek_reply_cwd();
            in_question
        } else {
            false
        };

        // Question mode is text-only on the wire; never attach/defer an image
        if in_question {
            return self.insert_pasted_caption(Some(text), true).0;
        }
        // Search is a text filter. Skip path reads and attachment probes so a slow disk cannot stall the UI future.
        if self.search_mode && !peek {
            return self.insert_pasted_caption(Some(text), false).0;
        }

        // Pasted text may be image file path(s) / `file://` URL(s) (drag-drop from Finder, or Copy on a file)
        // Resolve them synchronously and win; before deferring the clipboard probe, so a path
        // paste is not ALSO attached as the clipboard raster (no double insert)
        if !text.trim().is_empty() {
            let images = crate::prompt_images::try_read_images_from_paste(text);
            if !images.is_empty() {
                for img in images {
                    if peek {
                        self.attach_peek_pasted_image(img);
                    } else {
                        self.attach_pasted_image(img);
                    }
                }
                return InputOutcome::Changed;
            }
        }

        // Defer the clipboard image / file-url probe (osascript) off the event loop; the image wins over
        // the caption, so the caption is NOT inserted now; it lands on completion only if the probe finds
        // no image.
        let should_probe = text.len() < 4096 && !text.contains('\n');
        if probe_clipboard_attachments
            && should_probe
            && let Some(change_count) = crate::clipboard::attachment_probe_gate(Some(text))
        {
            self.enqueue_clipboard_attachment_probe(
                crate::app::actions::ClipboardPasteSource::BracketedDeferred {
                    text: text.to_owned(),
                },
                peek,
                change_count,
            );
            return InputOutcome::Changed;
        }

        if text.trim().is_empty() {
            return InputOutcome::Unchanged;
        }

        // Plain text paste (no raster).
        self.insert_pasted_caption(Some(text), peek).0
    }

    /// Attach a pasted image to the dispatch input as an `[Image #N]` chip.
    fn attach_pasted_image(
        &mut self,
        mut pasted: crate::prompt_images::PastedImage,
    ) -> (InputOutcome, crate::app::actions::ClipboardPasteCompletion) {
        let preparation = pasted.preview_preparation();
        // Dashboard prompts keep display metadata while durable paths remain session-owned.
        pasted.session_image_path = None;
        pasted.staged_temp_path = None;
        let completion = match self.dispatch.insert_image(pasted) {
            Ok(()) => {
                if let Some(preparation) = preparation {
                    self.pending_effects.push(
                        crate::app::actions::Effect::PreparePromptImagePreview { preparation },
                    );
                }
                crate::app::actions::ClipboardPasteCompletion::Handled
            }
            Err(msg) => {
                self.set_error_toast(&msg);
                crate::app::actions::ClipboardPasteCompletion::Failed(
                    crate::app::actions::ClipboardPasteFailure::AlreadyReported,
                )
            }
        };
        if self.search_mode {
            self.sync_search_filter_from_dispatch();
        }
        (InputOutcome::Changed, completion)
    }

    /// Insert a plain-text (caption) paste into the dispatch input (`peek = false`) or the peek reply (`peek = true`); the image / file-url portion
    /// of a paste is handled by the deferred probe.
    /// Whitespace-only text is a no-op (matches the bracketed arm; no stray spaces inserted).
    fn insert_pasted_caption(
        &mut self,
        text: Option<&str>,
        peek: bool,
    ) -> (InputOutcome, crate::app::actions::ClipboardTextInsertion) {
        use crate::app::actions::ClipboardTextInsertion;
        let Some(text) = text else {
            // Clipboard entirely empty; consume the key
            return (InputOutcome::Changed, ClipboardTextInsertion::Empty);
        };
        if text.trim().is_empty() {
            return (InputOutcome::Unchanged, ClipboardTextInsertion::Empty);
        }
        let event = if peek {
            self.peek_reply.handle_paste(text)
        } else {
            self.dispatch.handle_paste(text)
        };
        if !peek && matches!(event, PromptEvent::Edited) {
            if self.search_mode {
                self.sync_search_filter_from_dispatch();
            } else {
                self.dispatch.refresh_slash(&self.models);
            }
        }
        let completion = match event {
            PromptEvent::Edited => ClipboardTextInsertion::Inserted,
            PromptEvent::Ignored => ClipboardTextInsertion::Failed,
        };
        (InputOutcome::Changed, completion)
    }

    /// Enqueue attachment probing off-thread so paste-then-send remains ordered.
    fn enqueue_clipboard_attachment_probe(
        &mut self,
        source: crate::app::actions::ClipboardPasteSource,
        peek: bool,
        change_count: Option<u64>,
    ) {
        let target = if peek {
            // Callers only pass `peek = true` with the panel open; stamping the row lets the completion drop a paste whose panel closed/moved
            let Some(p) = self.peek.as_ref() else {
                return;
            };
            crate::app::actions::ClipboardPasteTarget::DashboardPeek { row: p.row.clone() }
        } else {
            crate::app::actions::ClipboardPasteTarget::DashboardDispatch
        };
        self.paste_probe_in_flight += 1;
        self.pending_effects
            .push(crate::app::actions::Effect::ProbeClipboardAttachment {
                ctx: crate::app::actions::ClipboardPasteContext { target, source },
                change_count,
            });
    }

    /// Attach a pasted image to the peek reply as an `[Image #N]` chip.
    fn attach_peek_pasted_image(
        &mut self,
        mut pasted: crate::prompt_images::PastedImage,
    ) -> (InputOutcome, crate::app::actions::ClipboardPasteCompletion) {
        if self.peek.as_ref().is_some_and(|p| p.question.is_some()) {
            return (
                InputOutcome::Unchanged,
                crate::app::actions::ClipboardPasteCompletion::Dropped,
            );
        }
        let preparation = pasted.preview_preparation();
        pasted.session_image_path = None;
        pasted.staged_temp_path = None;
        let completion = match self.peek_reply.insert_image(pasted) {
            Ok(()) => {
                if let Some(preparation) = preparation {
                    self.pending_effects.push(
                        crate::app::actions::Effect::PreparePromptImagePreview { preparation },
                    );
                }
                crate::app::actions::ClipboardPasteCompletion::Handled
            }
            Err(msg) => {
                self.set_error_toast(&msg);
                crate::app::actions::ClipboardPasteCompletion::Failed(
                    crate::app::actions::ClipboardPasteFailure::AlreadyReported,
                )
            }
        };
        (InputOutcome::Changed, completion)
    }

    /// Ctrl/Cmd+V into the dispatch input (`peek = false`) or the peek reply (`peek = true`): a pasted file path resolves synchronously and wins;
    /// else the clipboard raster/file-url probe defers off the event loop (the image wins over the caption, inserted on completion only if no image);
    /// else plain text with no raster inserts synchronously.
    fn handle_paste_key_deferred(
        &mut self,
        clipboard_text: crate::app::actions::ClipboardTextRead,
        peek: bool,
    ) -> InputOutcome {
        // The peek reply has extra preconditions the dispatch input does not.
        let in_question = if peek {
            let Some(p) = self.peek.as_ref() else {
                return InputOutcome::Unchanged;
            };
            let in_question = p.question.is_some();
            if in_question {
                let on_reject = p.selected_option.is_some() && p.reject_option == p.selected_option;
                if !on_reject {
                    return InputOutcome::Unchanged;
                }
            }
            // Pasting implies an intent to reply: focus the input and root the `@` picker
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            self.ensure_peek_reply_cwd();
            in_question
        } else {
            false
        };

        // Question mode is text-only on the wire; never attach/defer an image
        if in_question {
            return self
                .insert_pasted_caption(clipboard_text.as_deref(), true)
                .0;
        }
        if self.search_mode && !peek {
            return self
                .insert_pasted_caption(clipboard_text.as_deref(), false)
                .0;
        }

        // A pasted file path resolves synchronously and wins (drag-drop / Finder Cmd+C); before deferring, so it is not double-attached
        if let Some(text) = clipboard_text.as_deref()
            && !text.trim().is_empty()
        {
            let images = crate::prompt_images::try_read_images_from_paste(text);
            if !images.is_empty() {
                for img in images {
                    if peek {
                        self.attach_peek_pasted_image(img);
                    } else {
                        self.attach_pasted_image(img);
                    }
                }
                return InputOutcome::Changed;
            }
        }

        // An image is likely on the pasteboard, so defer; the image wins over the caption
        // (the caption is inserted on completion only if the probe finds no image)
        if let Some(change_count) =
            crate::clipboard::attachment_probe_gate(clipboard_text.as_deref())
        {
            self.enqueue_clipboard_attachment_probe(
                crate::app::actions::ClipboardPasteSource::ClipboardKey {
                    text: clipboard_text,
                    tip_showing: false,
                },
                peek,
                change_count,
            );
            return InputOutcome::Changed;
        }

        // No raster / lone URL: insert the plain text synchronously.
        self.insert_pasted_caption(clipboard_text.as_deref(), peek)
            .0
    }

    /// Attach the result of a deferred clipboard attachment probe ([`Effect::ProbeClipboardAttachment`]) to the surface named by `ctx.target`
    /// (dispatch input or peek reply).
    /// The heavy read/decode already ran off-thread; this only mutates input state on the event loop.
    pub(crate) fn complete_clipboard_attachment_paste(
        &mut self,
        ctx: crate::app::actions::ClipboardPasteContext,
        image: crate::app::actions::ProbedAttachment,
        file_urls: Option<String>,
    ) -> crate::app::actions::ClipboardPasteCompletion {
        use crate::app::actions::{
            ClipboardPasteCompletion, ClipboardPasteFailure, ClipboardPasteTarget, ProbedAttachment,
        };
        let peek_row = match &ctx.target {
            ClipboardPasteTarget::DashboardPeek { row } => Some(row.clone()),
            _ => None,
        };
        let peek = peek_row.is_some();
        self.paste_probe_in_flight = self.paste_probe_in_flight.saturating_sub(1);
        // Drop a peek completion if the panel closed OR moved to another row between enqueue and now, so the attachment can't land in the hidden
        // reply buffer or a different agent's reply (mirrors the agent's agent-id-gone guard in dispatch)
        if let Some(row) = &peek_row
            && self.peek.as_ref().is_none_or(|p| p.row != *row)
        {
            return ClipboardPasteCompletion::Dropped;
        }
        // A question that arrived on the peeked row mid-probe makes the reply text-only on the wire: attachments are discarded LOUDLY below (the
        // attach helper's silent question no-op would drop them with zero feedback), and the caption/wrap paths stay suppressed
        let peek_in_question = peek && self.peek.as_ref().is_some_and(|p| p.question.is_some());
        let text_on_miss = (!peek_in_question)
            .then(|| ctx.source.text_to_insert_on_miss(&image))
            .flatten();
        let mut attachment = match image {
            ProbedAttachment::Image(pasted) => {
                if peek_in_question {
                    self.set_error_toast("Pasted image discarded: reply switched to a question");
                    ClipboardPasteCompletion::Dropped
                } else {
                    let (_, completion) = if peek {
                        self.attach_peek_pasted_image(pasted)
                    } else {
                        self.attach_pasted_image(pasted)
                    };
                    completion
                }
            }
            ProbedAttachment::PersistFailed(msg) => {
                self.set_error_toast(&msg);
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AlreadyReported)
            }
            ProbedAttachment::NoRaster => ClipboardPasteCompletion::FullMiss,
            ProbedAttachment::ProbeDropped => ClipboardPasteCompletion::Dropped,
            ProbedAttachment::ProbeFailed => {
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AttachmentRead)
            }
        };
        if peek_in_question && attachment == ClipboardPasteCompletion::FullMiss {
            if file_urls.as_deref().is_some_and(|urls| {
                !crate::prompt_images::try_read_images_from_paste(urls).is_empty()
            }) {
                self.set_error_toast("Pasted image discarded: reply switched to a question");
            }
            attachment = ClipboardPasteCompletion::Dropped;
        }
        let file = if attachment == ClipboardPasteCompletion::FullMiss {
            file_urls.and_then(|urls| {
                let images = crate::prompt_images::try_read_images_from_paste(&urls);
                if images.is_empty() {
                    return None;
                }
                let mut completion =
                    ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AlreadyReported);
                for img in images {
                    let (_, inserted) = if peek {
                        self.attach_peek_pasted_image(img)
                    } else {
                        self.attach_pasted_image(img)
                    };
                    if inserted == ClipboardPasteCompletion::Handled {
                        completion = ClipboardPasteCompletion::Handled;
                    }
                }
                Some(completion)
            })
        } else {
            None
        };
        let text = text_on_miss.map(|text| self.insert_pasted_caption(Some(text), peek).1);
        let completion = crate::app::actions::reduce_clipboard_paste_completion(
            &ctx.source,
            attachment,
            file,
            text,
        );
        if completion == ClipboardPasteCompletion::FullMiss && ctx.source.is_clipboard_key() {
            crate::clipboard::log_paste_key_empty_host_clipboard(ctx.target.surface_str());
        }
        completion
    }

    /// After a deferred paste probe completes, take the sends stashed while the probe(s) were in flight
    /// (dispatch first, then peek), rebuilding each from its now-updated widget text so a freshly
    /// attached image chip (and its aligned range) travels with it.
    pub(crate) fn take_deferred_sends_after_paste(&mut self) -> Vec<crate::app::actions::Action> {
        if self.paste_probe_in_flight != 0 {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if let Some(DeferredDispatchSend { attach }) = self.deferred_dispatch_send.take() {
            actions.push(crate::app::actions::Action::DashboardDispatch {
                text: self.dispatch.text().to_string(),
                attach,
            });
        }
        if let Some(DeferredPeekSend { row, attach }) = self.deferred_peek_send.take() {
            let same_row = self.peek.as_ref().is_some_and(|p| p.row == row);
            let in_question = self.peek.as_ref().is_some_and(|p| p.question.is_some());
            if same_row && !in_question {
                actions.push(crate::app::actions::Action::DashboardPeekReply {
                    row,
                    text: self.peek_reply.text().to_string(),
                    attach,
                });
            } else if !same_row {
                // Never reply to a row the user is no longer peeking.
                self.set_error_toast("Reply canceled: peek panel changed");
            } else {
                // A question now owns the panel (Enter answers it there, and the reply dispatch would silently queue a prompt and wipe the draft
                // behind the dialog); drop the stash; the draft stays put
                self.set_error_toast("Reply canceled: answer the question first");
            }
        }
        actions
    }

    pub(crate) fn peek_owns_input(&self) -> bool {
        self.peek.is_some() && !self.search_mode
    }

    fn sync_search_filter_from_dispatch(&mut self) {
        if !self.search_mode {
            return;
        }
        let trimmed = self.dispatch.text().trim();
        self.filter = if trimmed.is_empty() {
            Filter::None
        } else {
            Filter::from_value(parse_filter(trimmed))
        };
        self.manual_scroll_active = false;
    }

    /// Handle a key while the peek panel is open.
    fn handle_peek_key(
        &mut self,
        key: &KeyEvent,
        from_registry: Option<crate::actions::ActionId>,
    ) -> Option<InputOutcome> {
        let dashboard_owned = from_registry.is_some();

        // Ctrl+C / Ctrl+D must reach the app-global quit handler (the double-press-to-quit fallback fires only when the view returns `Unchanged`)
        // Returning `Unchanged` here bubbles them up cleanly instead of letting them leak into (and be swallowed by) the reply widget (whose Ctrl+C
        // would clear the draft instead)
        if key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key) {
            return Some(InputOutcome::Unchanged);
        }

        // `@`-file-search intercept: while the reply's context picker dropdown is open it owns
        // Up/Down/PageUp-Down/Tab/Enter/Esc/ Ctrl+P-N (navigate.
        if self.peek_reply.file_search_visible() {
            match self.peek_reply_handle_key(key) {
                PromptEvent::Edited => {
                    return Some(InputOutcome::Changed);
                }
                PromptEvent::Ignored => {}
            }
        }

        // Empty peek reply: shortcuts help opens instead of typing.
        if matches!(
            from_registry,
            Some(crate::actions::ActionId::DashboardShortcutsHelp)
        ) && self.peek_reply.text().is_empty()
        {
            return Some(InputOutcome::Action(Action::DashboardOpenShortcutsHelp));
        }

        // Dashboard-owned chords fall through so the registry actions (Ctrl+X stop, Ctrl+T pin, Shift+↑/↓
        // reorder, Shift+Tab mode, …) and the hardcoded. CtrlCtrl+/ search toggle keep working with the panel
        // open.
        let is_typing_char = matches!(key.code, KeyCode::Char(_))
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
        let peek_owned = is_typing_char
            || matches!(key.code, KeyCode::Esc | KeyCode::Enter)
            || (matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab)
                && key.modifiers.is_empty());
        if (dashboard_owned && !peek_owned) || key!('/', CONTROL).matches(key) {
            return None;
        }

        // Ctrl+S is "send and open": send / queue the reply AND walk into the agent's detail view.
        if key!('s', CONTROL).matches(key) {
            let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                return Some(InputOutcome::Unchanged);
            };
            let reply_text = self.peek_reply.text().to_string();
            let has_question = self.peek.as_ref().is_some_and(|p| p.question.is_some());
            if has_question || reply_text.trim().is_empty() {
                return Some(InputOutcome::Action(Action::DashboardAttach(row)));
            }
            return Some(InputOutcome::Action(Action::DashboardPeekReply {
                row,
                text: reply_text,
                attach: true,
            }));
        }

        // Number keys 1–9 SELECT (toggle) the matching option when a question is showing; they no longer answer directly
        // Selecting focuses the panel so it becomes an answer surface (`Enter` then
        // answers); pressing the selected option's key again deselects it (back to navigation)
        if let KeyCode::Char(c) = key.code
            && let Some(d) = c.to_digit(10)
            && (1..=9).contains(&d)
            && self.peek.as_ref().is_some_and(|p| p.question.is_some())
        {
            let idx = (d - 1) as usize;
            let valid = self.peek.as_ref().is_some_and(|p| idx < p.options.len());
            if !valid {
                let n_opts = self.peek.as_ref().map(|p| p.options.len()).unwrap_or(0);
                self.set_error_toast(&format!("No such option (only {n_opts} available)"));
                return Some(InputOutcome::Changed);
            }
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
                p.selected_option = if p.selected_option == Some(idx) {
                    None
                } else {
                    Some(idx)
                };
            }
            return Some(InputOutcome::Changed);
        }

        // Esc: the peek is shown by default while a row is selected, so Esc UNSELECTS; first clearing a typed reply, then deselecting the row and
        // focusing the `+ New Agent` button (which closes the peek and brings back the new-session input)
        if matches!(key.code, KeyCode::Esc) {
            if !self.peek_reply.text().is_empty() {
                self.clear_peek_reply();
                return Some(InputOutcome::Changed);
            }
            self.set_peek(None);
            self.focus_new_agent_button();
            return Some(InputOutcome::Changed);
        }

        // Tab toggles focus between the reply input and the row list, mirroring the dispatch box's two-focus model
        // Unfocusing dims the panel border and hides the caret (see `render_peek_panel`)
        if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
            if let Some(p) = self.peek.as_mut() {
                p.focused = !p.focused;
            }
            return Some(InputOutcome::Changed);
        }

        // When a permission / ask-tool question is showing and the panel is focused it's an option picker.
        let vim_mode = crate::appearance::cache::load_vim_mode();
        let question_mode = self
            .peek
            .as_ref()
            .is_some_and(|p| p.focused && p.question.is_some() && !p.options.is_empty());
        if question_mode {
            let selected = self.peek.as_ref().and_then(|p| p.selected_option);
            let opt_count = self.peek.as_ref().map(|p| p.options.len()).unwrap_or(0);
            let on_reject = selected.is_some()
                && self
                    .peek
                    .as_ref()
                    .is_some_and(|p| p.reject_option == selected);
            match (key.code, selected) {
                (KeyCode::Up, None) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                (KeyCode::Down, None) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                (KeyCode::Enter, None) => {
                    let row = self.peek.as_ref().map(|p| p.row.clone());
                    return Some(
                        row.map(|r| InputOutcome::Action(Action::DashboardAttach(r)))
                            .unwrap_or(InputOutcome::Unchanged),
                    );
                }
                // Right (and vim `l`) open detail on the nav surface; without this the modal catch-all below swallows them
                (KeyCode::Right | KeyCode::Char('l'), None)
                    if key.modifiers.is_empty()
                        && (matches!(key.code, KeyCode::Right) || vim_mode) =>
                {
                    let row = self.peek.as_ref().map(|p| p.row.clone());
                    return Some(
                        row.map(|r| InputOutcome::Action(Action::DashboardAttach(r)))
                            .unwrap_or(InputOutcome::Unchanged),
                    );
                }
                (KeyCode::Up, Some(0)) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                (KeyCode::Up, Some(i)) => {
                    if let Some(p) = self.peek.as_mut() {
                        p.selected_option = Some(i - 1);
                    }
                    return Some(InputOutcome::Changed);
                }
                (KeyCode::Down, Some(i)) if i + 1 >= opt_count => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                (KeyCode::Down, Some(i)) => {
                    if let Some(p) = self.peek.as_mut() {
                        p.selected_option = Some(i + 1);
                    }
                    return Some(InputOutcome::Changed);
                }
                (KeyCode::Enter, Some(i)) => {
                    let is_ask = self.peek.as_ref().is_some_and(|p| p.is_ask_question());
                    if on_reject
                        && matches!(
                            self.peek_reply.try_element_interaction(key),
                            Some(crate::views::prompt_widget::ElementInteraction::Inlined)
                        )
                    {
                        return Some(InputOutcome::Changed);
                    }
                    // On the reject/free-text option with typed text: submit the ask "Other" answer, or the permission rejection and its feedback message
                    let feedback = on_reject
                        .then(|| self.peek_reply.text_without_image_chips())
                        .filter(|t| !t.trim().is_empty());
                    if let Some(text) = feedback {
                        let row = self.peek.as_ref().map(|p| p.row.clone());
                        let request_id = self.peek.as_ref().and_then(|p| p.request_id);
                        if let Some(row) = row {
                            if is_ask {
                                return Some(InputOutcome::Action(
                                    Action::DashboardQuestionAnswer {
                                        row,
                                        option_idx: None,
                                        freeform: text,
                                    },
                                ));
                            } else if let Some(request_id) = request_id {
                                return Some(InputOutcome::Action(
                                    Action::DashboardPermissionFollowup {
                                        row,
                                        request_id,
                                        text,
                                    },
                                ));
                            }
                        }
                        return Some(InputOutcome::Unchanged);
                    }
                    // Otherwise answer the selected option; `peek_number_key` routes to the permission / ask answer action by source
                    if let Some(action) = super::peek::peek_number_key(self, i + 1) {
                        return Some(InputOutcome::Action(action));
                    }
                    return Some(InputOutcome::Unchanged);
                }
                _ => {}
            }
            // Free-text feedback editing; only when the reject option is the selected one
            // Delegated to the reply widget so the feedback field gets the same editing surface (selection, word ops, chips) as the main reply line
            if on_reject && matches!(self.peek_reply_handle_key(key), PromptEvent::Edited) {
                return Some(InputOutcome::Changed);
            }
            // Modal while the question picker is up: consume any other key so it can't leak into the (hidden) reply row or row navigation
            return Some(InputOutcome::Unchanged);
        }

        let focused = self.peek.as_ref().map(|p| p.focused).unwrap_or(false);

        // BARE Up/Down switch the peeked agent ONLY while the reply is a navigation surface: either
        // unfocused (Tab moved focus to the rows) or empty.
        if key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
            && (!focused || self.peek_reply.text().is_empty())
        {
            return Some(InputOutcome::Action(match key.code {
                KeyCode::Up => Action::DashboardSelectPrev,
                _ => Action::DashboardSelectNext,
            }));
        }

        // Open detail (mirror of overlay Left-to-back)
        // Right: nav surface (unfocused or empty)
        // Vim `l`: unfocused only; focused reply must type literal `l`, same as focused `j`/`k`
        let open_detail = key.modifiers.is_empty()
            && match key.code {
                KeyCode::Right => !focused || self.peek_reply.text().is_empty(),
                KeyCode::Char('l') if vim_mode => !focused,
                _ => false,
            };
        if open_detail {
            let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                return Some(InputOutcome::Unchanged);
            };
            return Some(InputOutcome::Action(Action::DashboardAttach(row)));
        }

        if matches!(key.code, KeyCode::Enter) {
            let mod_enter = crate::input::is_mod_enter(key);
            if focused
                && !mod_enter
                && matches!(
                    self.peek_reply.try_element_interaction(key),
                    Some(crate::views::prompt_widget::ElementInteraction::Inlined)
                )
            {
                return Some(InputOutcome::Changed);
            }
            let enter_is_newline = focused
                && (compose_enter_is_newline(self.multiline_mode, mod_enter)
                    || crate::input::is_delivered_super_enter(key));
            if !enter_is_newline {
                let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                    return Some(InputOutcome::Unchanged);
                };
                if !focused && vim_mode {
                    if let Some(p) = self.peek.as_mut() {
                        p.focused = true;
                    }
                    return Some(InputOutcome::Changed);
                }
                let reply_text = self.peek_reply.text().to_string();
                if !focused || reply_text.trim().is_empty() {
                    return Some(InputOutcome::Action(Action::DashboardAttach(row)));
                }
                return Some(InputOutcome::Action(Action::DashboardPeekReply {
                    row,
                    text: reply_text,
                    attach: false,
                }));
            }
        }

        // Ctrl+M: ToggleMultiline is PromptFocused only, so hardcode here.
        if key!('m', CONTROL).matches(key) {
            return Some(InputOutcome::Action(Action::SetMultilineMode(
                !self.multiline_mode,
            )));
        }

        // Space is just text now (the peek is tied to selection, so there is no Space-to-close; use Esc to unselect)
        // It falls through to the reply editor below
        if focused {
            // Keys the widget ignores are still CONSUMED (`Unchanged`) so they can't leak into the hidden
            // dispatch input below; app-global shortcuts still fire off the `Unchanged` bubble-up.
            return Some(match self.peek_reply_handle_key(key) {
                PromptEvent::Edited => InputOutcome::Changed,
                PromptEvent::Ignored => InputOutcome::Unchanged,
            });
        }

        // Vim unfocused: j/k select rows; i focuses reply without typing `i`.
        if vim_mode && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('j') => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                KeyCode::Char('k') => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                KeyCode::Char('i') => {
                    if let Some(p) = self.peek.as_mut() {
                        p.focused = true;
                    }
                    return Some(InputOutcome::Changed);
                }
                _ => {}
            }
        }

        // Unfocused: non-vim printable focuses+types; vim swallows (focus via Enter/i/Tab).
        if is_typing_char {
            if vim_mode {
                return Some(InputOutcome::Unchanged);
            }
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            let _ = self.peek_reply_handle_key(key);
            return Some(InputOutcome::Changed);
        }

        // Every OTHER key is CONSUMED (`Unchanged`) rather than left to fall through.
        Some(InputOutcome::Unchanged)
    }

    /// Resolve the dispatch input's send action for the given `attach` flag. A `/command` always routes
    /// to the slash dispatcher (there's no session to "open"), so `attach` only affects the
    /// plain-dispatch path.
    fn dispatch_send_action(&mut self, attach: bool) -> InputOutcome {
        let text = self.dispatch.text().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            if let Some(id) = self.selected.clone() {
                return InputOutcome::Action(Action::DashboardAttach(id));
            }
            // An empty Enter acts on the focused actions-row item, whichever pane has focus, so a click on `Worktree` or
            // `Open Previous` never leaves Enter dead while the footer promises an action
            return self
                .focused_action_click()
                .map_or(InputOutcome::Unchanged, InputOutcome::Action);
        }
        self.last_send_at = Some(Instant::now());
        if trimmed.starts_with('/') {
            return InputOutcome::Action(Action::DashboardDispatchSlash { text });
        }
        InputOutcome::Action(Action::DashboardDispatch { text, attach })
    }

    /// List-focused `y`/`n` confirm for an already-armed delete (arming is via `Ctrl+X` / `[✗]`, not `d`).
    /// When the list isn't focused, disarming is left to the caller so a second `Ctrl+X` reaches the dispatcher.
    fn handle_delete_confirm_key(&mut self, key: &KeyEvent) -> Option<InputOutcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if !self.list_focused {
            return None;
        }
        self.armed_delete_row()?;
        if !key.modifiers.is_empty() {
            self.delete_confirm = None;
            return None;
        }
        match key.code {
            KeyCode::Char('y') => Some(InputOutcome::Action(Action::DashboardDelete)),
            KeyCode::Char('n') => {
                self.delete_confirm = None;
                Some(InputOutcome::Changed)
            }
            _ => {
                self.delete_confirm = None;
                None
            }
        }
    }

    /// Whether an empty Enter now is an echo of the last send.
    /// Key auto-repeat in older terminals, a bouncing key switch, and a double tap all produce this echo.
    fn empty_enter_echoes_send(&self) -> bool {
        self.dispatch.text().trim().is_empty()
            && self
                .last_send_at
                .is_some_and(|sent| send_echo_window_open(sent.elapsed()))
    }

    /// Enter with the list focused attaches the selected row or acts on the focused right-hand item.
    fn handle_list_focused_enter(&mut self) -> InputOutcome {
        if self.empty_enter_echoes_send() {
            return InputOutcome::Unchanged;
        }
        if let Some(id) = self.selected.clone() {
            return InputOutcome::Action(Action::DashboardAttach(id));
        }
        // Enter on a right-hand item acts like a click on it, draft or no draft
        // Only `+ New Agent` sends a typed draft
        if self.focused_new_agent_sends_draft() {
            return self.dispatch_send_action(false);
        }
        if let Some(action) = self.focused_action_click() {
            return InputOutcome::Action(action);
        }
        InputOutcome::Unchanged
    }

    /// Enter while the dispatch input is active confirms the search, expands a chip, or sends.
    /// Returns `None` when the prompt widget should insert Enter as a newline.
    fn handle_dispatch_enter(
        &mut self,
        key: &KeyEvent,
        slash_accepted_send: bool,
    ) -> Option<InputOutcome> {
        let mod_enter = crate::input::is_mod_enter(key);
        if self.search_mode {
            // Enter confirms the filter
            // The cleared query puts the dispatch input back on the ❯ prompt
            self.search_mode = false;
            self.dispatch.set_text("");
            return Some(InputOutcome::Changed);
        }
        // An accepted no-arg slash command always sends
        // PromptWidget inserts a newline for a delivered SUPER+Enter (Kitty)
        let enter_is_newline = !slash_accepted_send
            && (compose_enter_is_newline(self.multiline_mode, mod_enter)
                || crate::input::is_delivered_super_enter(key));
        // Only a real bare Enter expands paste and file chips
        // Apple Terminal rescue makes `is_mod_enter` true for a bare Enter that must send or insert a newline
        if !mod_enter
            && matches!(
                self.dispatch.try_element_interaction(key),
                Some(crate::views::prompt_widget::ElementInteraction::Inlined)
            )
        {
            self.dispatch.refresh_slash(&self.models);
            return Some(InputOutcome::Changed);
        }
        if enter_is_newline {
            None
        } else if self.empty_enter_echoes_send() {
            Some(InputOutcome::Unchanged)
        } else {
            Some(self.dispatch_send_action(false))
        }
    }

    fn handle_key(&mut self, key: &KeyEvent, registry: &ActionRegistry) -> InputOutcome {
        // Resolve the registry binding up-front; the toast / delete-confirm clear below needs to know
        // whether this key IS the stop key, and it must run before the peek intercept (the lookup itself
        // is a pure read; the action is honoured further down). Arrows / Ctrl combos always resolve.
        let vim_mode = crate::appearance::cache::load_vim_mode();
        let from_registry =
            registry.lookup_with_mode(key, crate::actions::When::DashboardFocused, vim_mode);

        // Clear `error_toast` on any keypress so it never lingers; kept for `Ctrl+X` so the arm path's own messaging survives its first press
        let is_stop_key = matches!(from_registry, Some(crate::actions::ActionId::DashboardStop));
        if !is_stop_key {
            self.error_toast = None;
        }

        // Disarm delete-confirm on any non-confirming key
        // Two gestures are preserved: `Ctrl+X` (its second press is the confirm, read
        // by the dispatcher) and a list-focused bare `y`/`n` (handled just below)
        let confirm_via_yn = self.list_focused
            && self.armed_delete_row().is_some()
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('n'));
        if !is_stop_key && !confirm_via_yn {
            self.delete_confirm = None;
        }

        if !is_stop_key && let Some(outcome) = self.handle_delete_confirm_key(key) {
            return outcome;
        }

        // Free-tier override: Ctrl+O opens the pinned upgrade CTA (when one is live) instead of falling
        // through to the dispatch input. Matched on the chord directly; `ToggleYolo` is
        // `When::AgentScreen`-scoped and never resolves here.
        if self.pinned_upgrade_cta_live && key!('o', CONTROL).matches(key) {
            return InputOutcome::Action(Action::AnnouncementsOpenCta(
                distill_telemetry::events::AnnouncementCtaSurface::Keyboard,
            ));
        }

        if crate::input::key::is_paste_key(key) {
            let clipboard_text = crate::app::actions::ClipboardTextRead::from_result(
                crate::clipboard::system_clipboard_read_text(),
            );
            return self.handle_paste_key_deferred(clipboard_text, self.peek_owns_input());
        }

        // Shift+Tab while the peek is open cycles the PEEKED agent's live mode, not the new-session staged
        // mode.
        if self.peek_owns_input()
            && matches!(
                from_registry,
                Some(crate::actions::ActionId::DashboardCycleMode)
            )
        {
            return InputOutcome::Action(Action::DashboardPeekCycleMode);
        }

        // Keys the panel doesn't own (registry-bound dashboard chords like. CtrlCtrl+X stop or Shift+↑/↓
        // reorder) return `None` and fall through so the dashboard's registry actions and global shortcuts
        // still fire.
        if self.peek_owns_input()
            && let Some(outcome) = self.handle_peek_key(key, from_registry)
        {
            return outcome;
        }

        let list_keys_active = self.list_keys_active();
        let vim_nav = vim_mode && self.list_focused && !self.search_mode;
        let step = match key.code {
            _ if !key.modifiers.is_empty() => None,
            KeyCode::Left if list_keys_active => Some(Step::Left),
            KeyCode::Right if list_keys_active => Some(Step::Right),
            KeyCode::Char('h') if vim_nav => Some(Step::Left),
            KeyCode::Char('l') if vim_nav => Some(Step::Right),
            _ => None,
        };

        // @-file-search intercept.
        if self.dispatch.file_search_visible() {
            match self.dispatch.handle_key(key) {
                PromptEvent::Edited => {
                    if self.search_mode {
                        self.sync_search_filter_from_dispatch();
                    } else {
                        self.dispatch.refresh_slash(&self.models);
                    }
                    return InputOutcome::Changed;
                }
                PromptEvent::Ignored => {
                    // Not a picker key; fall through to normal handling
                }
            }
        }

        // Special cases (Esc cascade, Enter dispatch) are handled below because they require multi-tier
        // behaviour (clear filter, clear input, exit) that a single registry action can't express.

        if let Some(step) = step
            && self.actions_focus.is_some()
        {
            if let Some(item) = self.actions_neighbour(step) {
                self.focus_action(item);
                return InputOutcome::Changed;
            }
            return InputOutcome::Unchanged;
        }

        if let Some(section) = self.selected_section {
            match (step, key.code) {
                (Some(Step::Right), _) => {
                    self.set_section_collapsed(section, false);
                    return InputOutcome::Changed;
                }
                (Some(Step::Left), _) => {
                    self.set_section_collapsed(section, true);
                    return InputOutcome::Changed;
                }
                // A section header is never a reply target, so with the input focused and a draft typed Enter falls through to
                // dispatch instead of toggling
                (None, KeyCode::Enter) if list_keys_active && key.modifiers.is_empty() => {
                    self.toggle_section(section);
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        if self.selected_idle_overflow {
            match (step, key.code) {
                (None, KeyCode::Enter) if list_keys_active && key.modifiers.is_empty() => {
                    self.toggle_idle_show_all();
                    return InputOutcome::Changed;
                }
                (Some(Step::Right), _) => {
                    self.idle_show_all = true;
                    return InputOutcome::Changed;
                }
                (Some(Step::Left), _) => {
                    self.idle_show_all = false;
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        // Short-terminal open (peek suppressed)
        if step == Some(Step::Right)
            && let Some(id) = self.selected.clone()
        {
            return InputOutcome::Action(Action::DashboardAttach(id));
        }

        // Never in search mode (there the buffer is a filter query, not a command). `slash_accepted_send`:
        // Enter accepted a terminal (no-arg) row and must fall through to dispatch; bypasses the multiline
        // Enter-as-newline swap.
        let mut slash_accepted_send = false;
        if self.dispatch.slash_open() && !self.search_mode {
            match key.code {
                KeyCode::Up => {
                    self.dispatch.slash_move_selection(-1);
                    return InputOutcome::Changed;
                }
                KeyCode::Down => {
                    self.dispatch.slash_move_selection(1);
                    return InputOutcome::Changed;
                }
                KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                    self.dispatch.slash_move_selection(-1);
                    return InputOutcome::Changed;
                }
                KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                    self.dispatch.slash_move_selection(1);
                    return InputOutcome::Changed;
                }
                KeyCode::Tab => {
                    // Accept records MRU and queues an off-thread persist internally
                    self.dispatch.accept_slash_completion(&self.models);
                    return InputOutcome::Changed;
                }
                KeyCode::Esc => {
                    self.dispatch.slash_close();
                    return InputOutcome::Changed;
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    // Accept the selected completion; if its insert text ends with a space the row "chains" (more input expected, e.g. `/model `),
                    // otherwise close the dropdown and fall through to the Enter handler which dispatches the slash command
                    let snap = self.dispatch.slash_snapshot();
                    let chains = snap
                        .selection()
                        .is_some_and(|row| row.insert_text.ends_with(' '));
                    // Accept records MRU and queues an off-thread persist internally
                    self.dispatch.accept_slash_completion(&self.models);
                    if chains {
                        return InputOutcome::Changed;
                    }
                    self.dispatch.slash_close();
                    slash_accepted_send = true;
                }
                _ => {}
            }
        }

        // `Ctrl+/` toggles search mode.
        if key!('/', CONTROL).matches(key) {
            if self.search_mode {
                self.exit_search_mode();
            } else {
                self.enter_search_mode();
            }
            return InputOutcome::Changed;
        }

        // Esc precedence: search, then peek, then clear filter, then unfocus the input. This sits between
        // the registry lookup and the action emission because. EscEsc is registered as `DashboardExit` but its
        // multi-tier behaviour can't be expressed as a single action; we expand it here.
        if matches!(key.code, KeyCode::Esc) {
            // Search mode owns Esc first: cancel the search (clear filter and query) and return to the dispatch prompt
            if self.search_mode {
                self.exit_search_mode();
                return InputOutcome::Changed;
            }
            if self.peek.is_some() {
                self.set_peek(None);
                return InputOutcome::Changed;
            }
            // An active filter clears next; the empty-state message promises "press Esc to clear the filter", so this stays ahead of the focus/exit
            // tiers regardless of which pane holds focus
            if self.filter.is_active() {
                self.filter = Filter::None;
                return InputOutcome::Changed;
            }
            // The typed draft is deliberately left intact; Esc never clears it (use. CtrlCtrl+U / Ctrl+C for that),
            // and it is retained if you close and reopen the dashboard within the same app process.
            if !self.list_focused {
                self.list_focused = true;
                // Re-engage selection-follow so the viewport tracks the cursor once the list takes focus (mirrors Tab)
                self.clear_manual_scroll();
                return InputOutcome::Changed;
            }
            // List focused: graduated back-out, then exit
            // The deselect tier keeps the "reply vs new session" contract; a selected row makes the dispatch input reply to it, and deselecting
            // flips it back to "new session" mode without leaving the dashboard
            if self.selected.is_some() {
                // Deselecting focuses the `+ New Agent` button so the cursor lands on a stable target instead of floating in `None`
                // The Enter-with-empty-prompt path keys off this to create-and-open, and the actions row paints the focused button in `accent_success` so the
                // user can see where the cursor went
                self.focus_new_agent_button();
                // Pure-viewport scroll state is bound to the cursor; once the cursor goes away, the next render frame should re-anchor without
                // snapping to a stale offset
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            if self.selected_section.is_some() {
                // Deselecting the section header focuses the `+ New Agent` button, mirroring the row-deselect tier above
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            if self.selected_idle_overflow {
                // Deselecting the Idle overflow toggle focuses the `+ New Agent` button, mirroring the section tier
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            // A right-hand actions-row item steps back to `+ New Agent` first; only from there does Esc exit
            if self
                .actions_focus
                .is_some_and(|item| item != ActionsFocus::NewAgent)
            {
                self.focus_new_agent_button();
                return InputOutcome::Changed;
            }
            return InputOutcome::Action(Action::ExitDashboard);
        }

        if let Some(id) = from_registry {
            let honor = match key.code {
                KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => list_keys_active,
                KeyCode::Char(_)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if id == crate::actions::ActionId::DashboardShortcutsHelp {
                        list_keys_active
                    } else {
                        self.list_focused && !self.search_mode
                    }
                }
                _ => true,
            };
            // Never let an auto-repeat (held key) drive the destructive. CtrlCtrl+X arm-then-confirm: holding the
            // key would arm and immediately confirm a delete. Require discrete presses, like the picker's `y`
            // confirm. Non-destructive actions may still repeat.
            if id == crate::actions::ActionId::DashboardStop && key.kind == KeyEventKind::Repeat {
                return InputOutcome::Unchanged;
            }
            if honor && let Some(outcome) = dashboard_action_for_id(id, &mut self.error_toast) {
                return outcome;
            }
        }

        // `/` is no longer special; it types a literal `/` into the prompt (handled by the widget fall-through below)
        // Filtering lives behind the explicit `Ctrl+/` search mode instead, so a dispatched prompt can start with `/`, `s:`, `a:`, or `#` without
        // being silently swallowed as a filter

        // Ctrl+S is "send and open": dispatch the prompt (or open the selected row / create from the button) AND walk straight into the detail view
        // This is the chord that replaced Shift+Enter (now freed, with Alt+Enter, for newline insertion)
        // No-op in search mode, where the buffer is a filter query, not a prompt
        if key!('s', CONTROL).matches(key) && !self.search_mode {
            return self.dispatch_send_action(true);
        }

        // Ctrl+M: ToggleMultiline is PromptFocused only, so hardcode here.
        if key!('m', CONTROL).matches(key) && !self.search_mode {
            return InputOutcome::Action(Action::SetMultilineMode(!self.multiline_mode));
        }

        if matches!(key.code, KeyCode::Enter) {
            if self.list_focused && key.modifiers.is_empty() {
                return self.handle_list_focused_enter();
            }
            if let Some(outcome) = self.handle_dispatch_enter(key, slash_accepted_send) {
                return outcome;
            }
        }

        // Shift+Tab (DashboardCycleMode) is resolved through the registry `from_registry` path above; its `ActionDef` carries the three terminal
        // encodings (`BackTab`, `BackTab`+SHIFT, `Tab`+SHIFT) as default/alt keys, so no hardcoded intercept is needed here

        // Tab toggles focus between the dispatch input and the overview list (the vim-style way to reach j/k navigation)
        // When the slash / `@` dropdowns are open the intercepts above already consumed Tab (accept completion), so this only fires otherwise
        if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
            self.set_list_focused(!self.list_focused);
            // Re-engage selection-follow so the viewport tracks the cursor once the list takes focus
            self.clear_manual_scroll();
            return InputOutcome::Changed;
        }

        // Overview focused: the input is inactive.
        if self.list_focused {
            if matches!(key.code, KeyCode::Char(_))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
            {
                if vim_mode {
                    if key.code == KeyCode::Char('i') && key.modifiers.is_empty() {
                        self.set_list_focused(false);
                        return InputOutcome::Changed;
                    }
                    return InputOutcome::Unchanged;
                }
                self.set_list_focused(false);
                // fall through to the widget so the char is typed.
            } else {
                // Non-printable (Backspace/Home/…) while the overview is focused must NOT leak into the inactive input
                return InputOutcome::Unchanged;
            }
        }

        // (error_toast already cleared at top of handle_key.)

        // Forward to the prompt widget (single-line).
        let old = self.dispatch.text().to_string();
        let had_highlight = self.dispatch.textarea.selection_range().is_some();
        let event = self.dispatch.handle_key(key);
        let dropped_highlight = had_highlight && self.dispatch.textarea.selection_range().is_none();
        let new = self.dispatch.text().to_string();
        if old != new {
            // Live-update the filter as the user types ONLY in search mode; the dispatch buffer is then the search query
            // Outside search mode the buffer is a dispatch prompt and never touches the filter (so `s:`/`a:`/`#`/`/` prefixes dispatch verbatim)
            // `parse_filter` still honours the `a:`/`s:`/`#` prefixes WITHIN search mode for power users; plain text is a substring match
            if self.search_mode {
                self.sync_search_filter_from_dispatch();
            } else {
                // Outside search mode the buffer is a dispatch prompt; refresh the slash snapshot so the `/command` dropdown opens / updates (and `@`
                // context) as the user types
                self.dispatch.refresh_slash(&self.models);
            }
            InputOutcome::Changed
        } else if event == PromptEvent::Edited || dropped_highlight {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }

    fn handle_mouse(&mut self, mouse: &crossterm::event::MouseEvent) -> InputOutcome {
        use crossterm::event::{MouseButton, MouseEventKind};

        self.last_mouse_pos = Some((mouse.column, mouse.row));

        // Update hover state when the mouse moves over a row or one of the header / actions-row affordances
        if matches!(mouse.kind, MouseEventKind::Moved) {
            let mut changed = self
                .new_agent_button_hit
                .update_hover(mouse.column, mouse.row);
            changed |= self
                .open_session_button_hit
                .update_hover(mouse.column, mouse.row);
            changed |= self
                .worktree_toggle_hit
                .update_hover(mouse.column, mouse.row);
            changed |= self.location_hit.update_hover(mouse.column, mouse.row);
            changed |= self.upgrade_cta_hit.update_hover(mouse.column, mouse.row);

            // Slash / @-file dropdown hover wins over row hover so the completion list
            // tracks the pointer while open (mirrors agent-view mouse handling in `app/mouse.rs`)
            if let Some(dd_area) = self.slash_dropdown_items_area {
                let has_scrollbar = self.slash_dropdown_hit.has_scrollbar;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);
                let new_hover =
                    if dd_area.contains((mouse.column, mouse.row).into()) && !on_scrollbar {
                        self.slash_dropdown_hit
                            .row_items
                            .get((mouse.row - dd_area.y) as usize)
                            .copied()
                    } else {
                        None
                    };
                changed |= self.dispatch.set_slash_hovered(new_hover);
            } else {
                changed |= self.dispatch.set_slash_hovered(None);
            }
            if let Some(dd_area) = self.file_search_dropdown_items_area {
                let fs = self.dropdown_file_search_mut();
                let result_count = fs.result_count();
                let scroll_offset = fs.scroll_offset();
                let has_scrollbar = result_count > dd_area.height as usize;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);
                let new_dd_hover =
                    if dd_area.contains((mouse.column, mouse.row).into()) && !on_scrollbar {
                        Some((mouse.row - dd_area.y) as usize + scroll_offset)
                    } else {
                        None
                    };
                changed |= fs.set_hovered(new_dd_hover);
            } else {
                changed |= self.dropdown_file_search_mut().set_hovered(None);
            }

            let new_hover = self
                .row_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone());
            if new_hover != self.hovered_row {
                self.hovered_row = new_hover;
                changed = true;
            }
            let new_hover_delete = self
                .row_delete_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone());
            if new_hover_delete != self.hovered_delete {
                self.hovered_delete = new_hover_delete;
                changed = true;
            }
            // Section-header hover: the renderer brightens its text
            let new_hover_section = self
                .section_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(key, _)| *key);
            if new_hover_section != self.hovered_section {
                self.hovered_section = new_hover_section;
                changed = true;
            }
            // Idle overflow row hover: the renderer brightens its text
            let new_hover_overflow = self.idle_overflow_rect.is_some_and(|r| {
                mouse.column >= r.x
                    && mouse.column < r.x + r.width
                    && mouse.row >= r.y
                    && mouse.row < r.y + r.height
            });
            if new_hover_overflow != self.hovered_idle_overflow {
                self.hovered_idle_overflow = new_hover_overflow;
                changed = true;
            }
            if changed {
                return InputOutcome::Changed;
            }
            return InputOutcome::Unchanged;
        }

        if self.peek_owns_input()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(rect) = self.peek_close_rect
            && mouse.column >= rect.x
            && mouse.column < rect.x + rect.width
            && mouse.row >= rect.y
            && mouse.row < rect.y + rect.height
        {
            // `set_peek(None)` enforces the peek/close-rect invariant atomically
            self.set_peek(None);
            return InputOutcome::Changed;
        }

        // Peek reply input: mouse interaction with the `❯ reply` row (or the reject-feedback slot in
        // question mode), mirroring the dispatch box's click-to-focus plus the agent prompt's drag text
        // selection.
        if self.peek_owns_input() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(rect) = self.peek_reply_rect
                        && mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                    {
                        if let Some(p) = self.peek.as_mut() {
                            p.focused = true;
                        }
                        let _ = self.peek_reply.handle_mouse(mouse);
                        let now = Instant::now();
                        if self.last_prompt_click.is_some_and(|last| {
                            now.duration_since(last).as_millis() < PROMPT_MULTI_CLICK_MS
                        }) {
                            let _ = self.peek_reply.expand_paste_element_at_cursor();
                        }
                        self.last_prompt_click = Some(now);
                        return InputOutcome::Changed;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                    if !self.peek_reply.text().is_empty() =>
                {
                    let _ = self.peek_reply.handle_mouse(mouse);
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        // Same Drag/Up treatment for the dispatch box, gated off while a peek is open (the dispatch input is hidden then) and in search mode (the
        // search line renders outside the textarea, matching the Down forwarding below)
        if self.peek.is_none()
            && !self.search_mode
            && !self.dispatch.text().is_empty()
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            let _ = self.dispatch.handle_mouse(mouse);
            return InputOutcome::Changed;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            // Slash dropdown click must run BEFORE row attach: the model list (and other arg suggestions) paints over agent rows, so without this a
            // mouse click falls through to `DashboardAttach` and opens the underlying session
            if let Some(dd_area) = self.slash_dropdown_items_area
                && dd_area.contains((mouse.column, mouse.row).into())
            {
                let snap = self.dispatch.slash_snapshot();
                let has_scrollbar = self.slash_dropdown_hit.has_scrollbar;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);

                if on_scrollbar {
                    let click_frac = (mouse.row - dd_area.y) as f64 / dd_area.height.max(1) as f64;
                    let target = (click_frac * snap.matches.len() as f64) as usize;
                    let max = snap.matches.len().saturating_sub(1);
                    let delta = target.min(max) as isize - snap.selected as isize;
                    self.dispatch.slash_move_selection(delta);
                    self.dispatch.slash_preview_current_selection();
                } else if let Some(&item_idx) = self
                    .slash_dropdown_hit
                    .row_items
                    .get((mouse.row - dd_area.y) as usize)
                {
                    self.dispatch.set_slash_hovered(Some(item_idx));
                    if self.dispatch.select_slash_hovered() {
                        self.dispatch.slash_commit_preview();
                        self.dispatch.accept_slash_completion(&self.models);
                    }
                }
                self.set_list_focused(false);
                return InputOutcome::Changed;
            }

            // File-search (`@`) dropdown click, same priority as slash: absorb the hit so it never attaches a session row under the list
            // Uses the on-screen picker (peek reply while peeking, otherwise the dispatch box)
            if let Some(dd_area) = self.file_search_dropdown_items_area
                && dd_area.contains((mouse.column, mouse.row).into())
            {
                let peek_visible = self.peek_owns_input();
                let accepted = {
                    let fs = self.dropdown_file_search_mut();
                    let result_count = fs.result_count();
                    let scroll_offset = fs.scroll_offset();
                    let has_scrollbar = result_count > dd_area.height as usize;
                    let on_scrollbar = has_scrollbar
                        && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);
                    if on_scrollbar {
                        let click_frac =
                            (mouse.row - dd_area.y) as f64 / dd_area.height.max(1) as f64;
                        let target = (click_frac * result_count as f64) as usize;
                        let max = result_count.saturating_sub(1);
                        let selected = fs.selected();
                        fs.move_selection(target.min(max) as isize - selected as isize);
                        false
                    } else {
                        let row_idx = (mouse.row - dd_area.y) as usize + scroll_offset;
                        fs.set_hovered(Some(row_idx));
                        fs.select_hovered()
                    }
                };
                if accepted {
                    if peek_visible {
                        self.peek_reply.accept_file_search_result();
                    } else {
                        self.dispatch.accept_file_search_result();
                        self.sync_search_filter_from_dispatch();
                    }
                }
                self.set_list_focused(false);
                return InputOutcome::Changed;
            }

            if let Some(id) = self
                .row_delete_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone())
            {
                self.manual_scroll_active = false;
                // Second `[✗]` click within the window confirms; else re-arm.
                if self.armed_delete_row().as_ref() == Some(&id) {
                    return InputOutcome::Action(Action::DashboardDelete);
                }
                self.focus_row(id.clone());
                self.set_list_focused(true);
                self.arm_delete(id);
                return InputOutcome::Changed;
            }

            // Click on the `+ New Agent` button: same outcome as Enter-with-empty-prompt while the button is focused
            // Routed through the dispatcher so the create-session and view-switch sequence stays in one place
            // The hit test runs BEFORE the row-click check so the button can sit inside the actions row without competing for clicks
            if self.new_agent_button_hit.contains(mouse.column, mouse.row) {
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail);
            }

            if self
                .open_session_button_hit
                .contains(mouse.column, mouse.row)
            {
                self.focus_open_session_button();
                self.set_list_focused(true);
                self.manual_scroll_active = false;
                return InputOutcome::Action(Action::ShowSessionPicker);
            }

            if self.worktree_toggle_hit.contains(mouse.column, mouse.row) {
                self.focus_action(ActionsFocus::Worktree);
                return InputOutcome::Action(Action::DashboardToggleWorktree);
            }

            // A click on the header upgrade CTA `[label]` opens the promo url (resolved through the slot gate at dispatch time)
            if self.upgrade_cta_hit.contains(mouse.column, mouse.row) {
                return InputOutcome::Action(Action::AnnouncementsOpenCta(
                    distill_telemetry::events::AnnouncementCtaSurface::Dashboard,
                ));
            }

            // A click on the header location label opens the location picker
            // Checked after the actions-row buttons; the label lives on the header row above them
            if self.location_hit.contains(mouse.column, mouse.row) {
                return InputOutcome::Action(Action::DashboardOpenLocationPicker);
            }

            // A click on a section header selects it and toggles collapse
            // Checked before the row hit-test (separate layout region).
            if let Some(key) = self
                .section_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(k, _)| *k)
            {
                self.focus_section(key);
                self.toggle_section(key);
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }

            // A click on the Idle "N more" overflow row focuses it and flips show-all (mirrors the section-header click)
            // Checked before the row hit-test; the overflow row is never in `row_rects`
            if let Some(rect) = self.idle_overflow_rect
                && mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
            {
                self.focus_idle_overflow();
                self.toggle_idle_show_all();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }

            // Single-click attaches (opens the conversation popup) immediately; keyboard navigation (↑/↓) is the way to browse without attaching
            if let Some(id) = self
                .row_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone())
            {
                // Clicking a row is selection-driven, so re-engage the clamp's snap-to-selection by clearing the manual-scroll flag
                // Without this, a click after a wheel-scroll would jump the viewport on the next frame (the bias-up pull-back kicks in)
                self.manual_scroll_active = false;
                // `focus_row` also clears `actions_focus` so the two cursor states stay mutually exclusive (clicking a row while the
                // button was focused hands the cursor over to the row)
                self.focus_row(id.clone());
                self.last_click = Some((id.clone(), Instant::now()));
                return InputOutcome::Action(Action::DashboardAttach(id));
            }

            // Click anywhere on the dispatch input box focuses it; i.e. clears `list_focused`. This must work
            // whether or not vim mode is on: in vim mode the overview steals the keyboard (j/k nav), so
            // without this a mouse user clicking the box would be stranded with no caret.
            if let Some(rect) = self.dispatch_rect
                && mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
            {
                self.set_list_focused(false);
                // Forward the click so the caret lands where the user clicked
                // Skipped in search mode, where the prompt renders its own single-line
                // cursor with a `Search:` prefix rather than through the textarea
                if !self.search_mode {
                    let _ = self.dispatch.handle_mouse(mouse);
                    let now = Instant::now();
                    if self.last_prompt_click.is_some_and(|last| {
                        now.duration_since(last).as_millis() < PROMPT_MULTI_CLICK_MS
                    }) && self.dispatch.expand_paste_element_at_cursor()
                    {
                        self.dispatch.refresh_slash(&self.models);
                    }
                    self.last_prompt_click = Some(now);
                }
                return InputOutcome::Changed;
            }
        }

        InputOutcome::Unchanged
    }

    /// Route input to the open location picker.
    fn handle_location_picker_input(&mut self, ev: &Event) -> InputOutcome {
        // Mouse is handled separately so the `location_picker` borrow below doesn't overlap the `&mut self` mouse helper
        if let Event::Mouse(mouse) = ev {
            return self.handle_location_picker_mouse(mouse);
        }

        let Some(lp) = self.location_picker.as_mut() else {
            return InputOutcome::Unchanged;
        };

        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
        {
            lp.refresh_suggestions();
            return match lp.chosen_input() {
                Some(input) => InputOutcome::Action(Action::DashboardChangeLocation { input }),
                None => InputOutcome::Unchanged,
            };
        }

        // Tab does shell-style completion: fill the input with the selected row's path (tilde-collapsed)
        // plus a trailing `/` so the next listing drills into it
        // No-op when nothing is selected
        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
            && key.code == KeyCode::Tab
            && key.modifiers.is_empty()
        {
            let visible = lp.visible_candidates();
            let Some(c) = visible.get(lp.picker.selected) else {
                return InputOutcome::Unchanged;
            };
            let mut filled = crate::recent_dirs::display_path(&c.path);
            if !filled.ends_with('/') {
                filled.push('/');
            }
            lp.picker.set_query(filled);
            lp.picker.selected = 0;
            lp.picker.scroll_offset = None;
            // The path changed; drop any stale "Not a directory" error
            lp.error = None;
            lp.refresh_suggestions();
            return InputOutcome::Changed;
        }

        let entry_count = lp.visible_candidates().len();
        let config = location_picker_config();
        let outcome =
            crate::views::picker::handle_picker_input(ev, &mut lp.picker, entry_count, &config);
        // When the user edits the path, drop the stale validation error so a corrected (possibly valid) path isn't shown next to a red "Not a
        // directory" left over from the previous failed attempt
        if matches!(&outcome, crate::views::picker::PickerOutcome::QueryChanged) {
            lp.error = None;
            // Re-list only when the edited path changes; cursor motion is redraw-only.
            lp.refresh_suggestions();
        }
        match outcome {
            crate::views::picker::PickerOutcome::Closed => {
                InputOutcome::Action(Action::DashboardCloseLocationPicker)
            }
            crate::views::picker::PickerOutcome::Changed
            | crate::views::picker::PickerOutcome::QueryChanged => InputOutcome::Changed,
            _ => InputOutcome::Unchanged,
        }
    }

    /// Mouse handling for the open location picker: modal chrome (close button / click-outside) first, then content-row click (select and apply) and
    /// hover (move the cursor).
    fn handle_location_picker_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
    ) -> InputOutcome {
        use crate::views::modal_window::{ModalWindowOutcome, handle_modal_mouse};
        use crossterm::event::{MouseButton, MouseEventKind};

        let Some(lp) = self.location_picker.as_mut() else {
            return InputOutcome::Unchanged;
        };

        match handle_modal_mouse(&mut lp.window, mouse.kind, mouse.column, mouse.row) {
            ModalWindowOutcome::CloseRequested => {
                return InputOutcome::Action(Action::DashboardCloseLocationPicker);
            }
            ModalWindowOutcome::Handled => return InputOutcome::Changed,
            _ => {}
        }

        // Worktree toggle button on the path row: a left-click flips `worktree_mode`, which
        // (when the location is applied) arms the dashboard's worktree dispatch
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && lp.worktree_hit.contains(mouse.column, mouse.row)
        {
            lp.worktree_mode = !lp.worktree_mode;
            return InputOutcome::Changed;
        }

        // Map the cursor to a content row (visible-list index) without holding the `content_hits` borrow past the lookup
        let hit_idx: Option<usize> = lp.content_hits.as_ref().and_then(|hits| {
            hits.item_rects.iter().enumerate().find_map(|(i, r)| {
                let inside = mouse.column >= r.x
                    && mouse.column < r.x + r.width
                    && mouse.row >= r.y
                    && mouse.row < r.y + r.height;
                inside.then(|| hits.entry_indices.get(i).copied().unwrap_or(i))
            })
        });

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(entry_idx) = hit_idx {
                    let visible = lp.visible_candidates();
                    if let Some(c) = visible.get(entry_idx) {
                        let input = c.path.to_string_lossy().into_owned();
                        return InputOutcome::Action(Action::DashboardChangeLocation { input });
                    }
                }
                InputOutcome::Unchanged
            }
            MouseEventKind::Moved => {
                // Brighten the worktree button when hovered (its rect was set on the prior render)
                let wt_changed = lp.worktree_hit.update_hover(mouse.column, mouse.row);
                let row_changed = match hit_idx {
                    Some(entry_idx) if lp.picker.selected != entry_idx => {
                        lp.picker.selected = entry_idx;
                        true
                    }
                    _ => false,
                };
                if wt_changed || row_changed {
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            _ => InputOutcome::Unchanged,
        }
    }

    /// Route input to the worktree-label dialog while it is open.
    fn handle_worktree_dialog_input(&mut self, ev: &Event) -> InputOutcome {
        use crate::app::app_view::NewWorktreeDialogOutcome;
        let Some(dialog) = self.worktree_dialog.as_mut() else {
            return InputOutcome::Unchanged;
        };
        let outcome = match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => dialog.handle_key(key),
            Event::Paste(text) => dialog.insert_paste(text),
            _ => return InputOutcome::Unchanged,
        };
        match outcome {
            NewWorktreeDialogOutcome::Submitted(label) => {
                self.worktree_dialog = None;
                InputOutcome::Action(Action::DashboardConfirmWorktree { label })
            }
            NewWorktreeDialogOutcome::Cancelled => {
                self.worktree_dialog = None;
                // Don't silently discard the user's typed prompt; restore the stashed prompt (from the prompt-send
                // path) to the dispatch input so they can resend it instead of losing it.
                if let Some(prompt) = self.pending_worktree_prompt.take() {
                    self.dispatch.restore(prompt);
                }
                InputOutcome::Changed
            }
            NewWorktreeDialogOutcome::Changed => InputOutcome::Changed,
            NewWorktreeDialogOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    /// Route a top-level event to the open shortcuts cheatsheet modal.
    fn handle_shortcuts_modal_input(&mut self, ev: &Event) -> InputOutcome {
        use crate::views::shortcuts_help::{
            ModalKeyOutcome, ShortcutsHelpOutcome, handle_modal_key, handle_mouse, handle_paste,
            toggle_membership,
        };

        let Some(modal) = self.shortcuts_modal.as_mut() else {
            return InputOutcome::Unchanged;
        };
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match handle_modal_key(
                    key,
                    &modal.entries,
                    &mut modal.state,
                    &mut modal.window,
                    modal.filter_active,
                    &modal.collapsed_sections,
                    &modal.expanded_ids,
                    &mut modal.mode,
                    /* compact */ false,
                ) {
                    ModalKeyOutcome::Close => {
                        InputOutcome::Action(Action::DashboardCloseShortcutsHelp)
                    }
                    ModalKeyOutcome::ToggleFilter => {
                        modal.filter_active = !modal.filter_active;
                        modal.state.selected = 0;
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::ToggleSection(idx) => {
                        toggle_membership(&mut modal.collapsed_sections, idx);
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::ToggleExpand(action_id) => {
                        toggle_membership(&mut modal.expanded_ids, action_id);
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::Changed => InputOutcome::Changed,
                    ModalKeyOutcome::Unchanged => InputOutcome::Unchanged,
                }
            }
            Event::Mouse(mouse) => {
                // Chrome first: the `[✗]` close button (click and hover brightening) and click-outside-to-close
                // live on the modal-window chrome, not in the picker content.
                match crate::views::modal_window::handle_modal_mouse(
                    &mut modal.window,
                    mouse.kind,
                    mouse.column,
                    mouse.row,
                ) {
                    crate::views::modal_window::ModalWindowOutcome::CloseRequested => {
                        return InputOutcome::Action(Action::DashboardCloseShortcutsHelp);
                    }
                    crate::views::modal_window::ModalWindowOutcome::Handled => {
                        return InputOutcome::Changed;
                    }
                    // Everything else (incl. wheel) belongs to the picker content below.
                    _ => {}
                }
                match handle_mouse(
                    mouse,
                    &modal.entries,
                    &mut modal.state,
                    modal.filter_active,
                    &modal.collapsed_sections,
                    &mut modal.mode,
                ) {
                    ShortcutsHelpOutcome::Close => {
                        InputOutcome::Action(Action::DashboardCloseShortcutsHelp)
                    }
                    ShortcutsHelpOutcome::ToggleFilter => {
                        modal.filter_active = !modal.filter_active;
                        modal.state.selected = 0;
                        InputOutcome::Changed
                    }
                    ShortcutsHelpOutcome::ToggleSection(idx) => {
                        toggle_membership(&mut modal.collapsed_sections, idx);
                        InputOutcome::Changed
                    }
                    // Unreachable today: handle_mouse never yields ToggleExpand (a row click opens detail); kept for exhaustiveness.
                    ShortcutsHelpOutcome::ToggleExpand(action_id) => {
                        toggle_membership(&mut modal.expanded_ids, action_id);
                        InputOutcome::Changed
                    }
                    ShortcutsHelpOutcome::Changed => InputOutcome::Changed,
                    ShortcutsHelpOutcome::Unchanged => InputOutcome::Unchanged,
                }
            }
            Event::Paste(text) => match handle_paste(text, &mut modal.state, &modal.mode) {
                ShortcutsHelpOutcome::Changed => InputOutcome::Changed,
                _ => InputOutcome::Unchanged,
            },
            _ => InputOutcome::Unchanged,
        }
    }

    /// Clamp the cursor to a still-visible row when the previous selection has disappeared.
    /// A deliberately-cleared selection (`None`) is PRESERVED: `None` is a valid steady state.
    /// The contract: no row selected means dispatch creates a new session; a selected row means dispatch replies to that agent.
    pub fn reanchor_selection(&mut self, rows: &[DashboardRow]) {
        // Section-cursor validation: a selected section header can go stale without any grouping toggle: a
        // `s:state` filter suppresses all state headers, and row churn (the last Working agent going idle)
        // removes the header outright.
        let focusables = super::render::focusables(
            rows,
            self.grouping,
            &self.filter,
            &self.collapsed_sections,
            self.idle_show_all,
            self.search_mode,
        );
        if let Some(key) = self.selected_section {
            let alive = focusables
                .iter()
                .any(|f| matches!(f, Focusable::Section(k) if *k == key));
            if !alive {
                self.focus_new_agent_button();
            }
        }
        // Idle-overflow cursor validation: the "N more" toggle row disappears when the Idle group shrinks below the cap (or the user expands it,
        // which removes the fold)
        // A stranded cursor there would leave Enter/footer hints acting on nothing, so fall back to the `+ New Agent` button
        if self.selected_idle_overflow
            && !focusables
                .iter()
                .any(|f| matches!(f, Focusable::IdleOverflow))
        {
            self.focus_new_agent_button();
        }
        // Row-cursor visibility: a selected row can vanish WITHOUT leaving `rows`: state churn can migrate
        // it into a collapsed section (e.g. a selected Idle row starts Working while "Working" is
        // collapsed).
        if let Some(sel) = self.selected.clone() {
            let visible = focusables
                .iter()
                .any(|f| matches!(f, Focusable::Row(id) if *id == sel));
            if !visible
                && let Some(key) =
                    super::render::section_of_row(rows, self.grouping, &self.filter, &sel)
            {
                self.focus_section(key);
            }
        }
        // Skip non-selectable placeholders ("… N more").
        let selectable: Vec<&DashboardRow> =
            rows.iter().filter(|r| !r.is_more_placeholder).collect();
        if selectable.is_empty() {
            self.selected = None;
            self.delete_confirm = None;
            return;
        }
        if let Some(sel) = self.selected.as_ref()
            && !selectable.iter().any(|r| r.id == *sel)
        {
            // The previously selected row was filtered out / closed / lost its parent
            // Drop the cursor; re-selecting is the user's job
            self.selected = None;
        }
        self.sync_delete_confirm_to_selection();
    }
}

/// Strict Enter swap for compose surfaces (agent prompt parity).
/// Multiline off: Shift/Alt (or rescued) Enter inserts a newline.
/// Multiline on: bare Enter inserts a newline; Shift/Alt sends/creates/opens.
fn compose_enter_is_newline(multiline: bool, mod_enter: bool) -> bool {
    multiline != mod_enter
}

/// Exhaustive map from `ActionId` to `InputOutcome` for dashboard-focused actions. Adding a new
/// `Dashboard*` `ActionId` without wiring it here is a compile error (there is no `_` arm). Returns
/// `None` for non-dashboard `ActionId`s; the caller falls through to widget input.
fn dashboard_action_for_id(
    id: crate::actions::ActionId,
    error_toast: &mut Option<String>,
) -> Option<InputOutcome> {
    use crate::actions::ActionId;
    match id {
        ActionId::DashboardSelectNext => Some(InputOutcome::Action(Action::DashboardSelectNext)),
        ActionId::DashboardSelectPrev => Some(InputOutcome::Action(Action::DashboardSelectPrev)),
        ActionId::DashboardTogglePin => Some(InputOutcome::Action(Action::DashboardTogglePin)),
        ActionId::DashboardBeginRename => Some(InputOutcome::Action(Action::DashboardBeginRename)),
        ActionId::DashboardStop => Some(InputOutcome::Action(Action::DashboardStop)),
        ActionId::DashboardCycleMode => Some(InputOutcome::Action(Action::DashboardCycleMode)),
        ActionId::DashboardToggleAutoApprove => {
            Some(InputOutcome::Action(Action::DashboardToggleAutoApprove))
        }
        ActionId::DashboardToggleWorktree => {
            Some(InputOutcome::Action(Action::DashboardToggleWorktree))
        }
        ActionId::DashboardOpenLocationPicker => {
            Some(InputOutcome::Action(Action::DashboardOpenLocationPicker))
        }
        ActionId::DashboardToggleGrouping => {
            Some(InputOutcome::Action(Action::DashboardToggleGrouping))
        }
        ActionId::DashboardReorderUp => Some(InputOutcome::Action(Action::DashboardReorderUp)),
        ActionId::DashboardReorderDown => Some(InputOutcome::Action(Action::DashboardReorderDown)),
        ActionId::DashboardShortcutsHelp => {
            // Opens the cheatsheet modal (mirroring the agent view's `ActiveModal::ShortcutsHelp`) via the action dispatcher so the build-entries
            // side-effect lives next to the other dashboard dispatchers in `app::dispatch`
            let _ = error_toast;
            Some(InputOutcome::Action(Action::DashboardOpenShortcutsHelp))
        }
        ActionId::DashboardExit => {
            // Esc is handled in the cascade above
            // A user-rebind of exit (e.g. F1) lands here.
            Some(InputOutcome::Action(Action::ExitDashboard))
        }
        // Non-dashboard ActionIds: fall through to the widget
        // This is the ONLY arm that should ever match; the compiler will flag a missing case when a new Dashboard* action is added
        ActionId::SendPrompt
        | ActionId::InterjectPrompt
        | ActionId::StashPrompt
        | ActionId::ScrollUp
        | ActionId::ScrollDown
        | ActionId::PageUp
        | ActionId::PageDown
        | ActionId::HalfPageUp
        | ActionId::HalfPageDown
        | ActionId::GotoTop
        | ActionId::GotoBottom
        | ActionId::SelectNext
        | ActionId::SelectPrev
        | ActionId::NextTurn
        | ActionId::PrevTurn
        | ActionId::NextResponse
        | ActionId::PrevResponse
        | ActionId::Collapse
        | ActionId::Expand
        | ActionId::ToggleFold
        | ActionId::ToggleExpandAll
        | ActionId::ExpandAllThinking
        | ActionId::ToggleRaw
        | ActionId::ToggleMouseCapture
        | ActionId::NextModel
        | ActionId::CancelTurn
        | ActionId::ToggleYolo
        | ActionId::ToggleMultiline
        | ActionId::FocusPrompt
        | ActionId::FocusScrollback
        | ActionId::CopyBlockContent
        | ActionId::CopyBlockMeta
        | ActionId::OpenBlockViewer
        | ActionId::OpenNextLink
        | ActionId::OpenPrevLink
        | ActionId::ToggleTodos
        | ActionId::ToggleTasks
        | ActionId::EditPromptExternal
        | ActionId::ToggleQueue
        | ActionId::OpenSessions
        | ActionId::OpenExtensions
        | ActionId::SendToBackground
        | ActionId::CycleMode
        | ActionId::BashMode
        | ActionId::Rewind
        | ActionId::KillBgTask
        | ActionId::DumpInputLog
        | ActionId::Quit
        | ActionId::NewSession
        | ActionId::ExitSession
        | ActionId::NewSessionInWorktree
        | ActionId::CommandPalette
        | ActionId::ModelPicker
        | ActionId::ShortcutsHelp
        | ActionId::OpenSettings
        | ActionId::OpenDashboard
        | ActionId::EnableVoiceMode
        | ActionId::VoiceToggle
        // Overlay actions are intercepted at the AppView level before they reach the dashboard's own input loop; they can never arrive here
        | ActionId::DashboardOverlayExit
        | ActionId::DashboardOverlayPrev
        | ActionId::DashboardOverlayNext
        | ActionId::DashboardOverlayStop => None,
    }
}

fn handle_rename_key(draft: &mut RenameDraft, key: &KeyEvent) -> InputOutcome {
    use crate::input::key::is_altgr;
    match key.code {
        KeyCode::Esc => return InputOutcome::Action(Action::DashboardCancelRename),
        KeyCode::Enter if key.modifiers.is_empty() => {
            return InputOutcome::Action(Action::DashboardCommitRename);
        }
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::CONTROL) && !is_altgr(key.modifiers) =>
        {
            return InputOutcome::Action(Action::DashboardCancelRename);
        }
        _ => {}
    }

    let can_insert = draft.text().chars().count() < MAX_RENAME_SCALARS;
    let outcome = draft
        .editor
        .handle_key_with_insert_policy(key, |character| {
            can_insert && rename_character_allowed(character)
        });
    rename_edit_outcome(outcome)
}

fn handle_rename_paste(draft: &mut RenameDraft, text: &str) -> InputOutcome {
    let remaining = MAX_RENAME_SCALARS.saturating_sub(draft.text().chars().count());
    let outcome =
        draft
            .editor
            .insert_paste_with_policy(text, rename_wire_character_allowed, remaining);
    rename_edit_outcome(outcome)
}

fn rename_edit_outcome(outcome: LineEditOutcome) -> InputOutcome {
    match outcome {
        LineEditOutcome::TextChanged
        | LineEditOutcome::HandledNoChange
        | LineEditOutcome::CursorChanged => InputOutcome::Changed,
        LineEditOutcome::Unhandled => InputOutcome::Unchanged,
    }
}

/// Parse a filter expression from the dispatch input. Unknown values fall back to substring.
pub fn parse_filter(text: &str) -> FilterValue {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("a:") {
        let rest = rest.trim();
        if rest.is_empty() {
            FilterValue::None
        } else {
            FilterValue::Agent(rest.to_string())
        }
    } else if let Some(rest) = trimmed.strip_prefix("s:") {
        let rest = rest.trim();
        if rest.is_empty() {
            // `s:` empty is consistent with `a:` empty: both clear the filter
            FilterValue::None
        } else if let Some(rs) = parse_row_state_token(rest) {
            FilterValue::State(rs)
        } else {
            // Unknown state token falls back to substring on the full `s:foobar` so the user sees feedback (their typed text is matched against
            // labels) AND realises the state path didn't take effect
            FilterValue::Substring(rest.to_string())
        }
    } else if let Some(rest) = trimmed.strip_prefix('#') {
        // `#<n>` keeps the `#` in the substring needle so it never matches arbitrary digits in labels
        // PR filtering is reserved for a future revision
        FilterValue::Substring(format!("#{rest}"))
    } else {
        FilterValue::Substring(trimmed.to_string())
    }
}

/// Parse a `RowState` from a user token.
/// Accepts common synonyms.
pub fn parse_row_state_token(s: &str) -> Option<RowState> {
    let normalised: String = s
        .chars()
        .filter_map(|c| {
            if c == '-' || c == '_' || c == ' ' {
                None
            } else {
                Some(c.to_ascii_lowercase())
            }
        })
        .collect();
    match normalised.as_str() {
        "needsinput" | "needs" | "input" => Some(RowState::NeedsInput),
        "working" | "busy" | "running" => Some(RowState::Working),
        "idle" => Some(RowState::Idle),
        "inactive" | "dormant" => Some(RowState::Inactive),
        "completed" | "done" => Some(RowState::Completed),
        "failed" | "errored" | "cancelled" | "canceled" => Some(RowState::Failed),
        _ => None,
    }
}

/// Read the persisted `[dashboard].enabled` flag (defaults to `true`).
///
/// Lenient: any error or unparseable value returns `None`, which the caller interprets as "use the default".
pub fn load_persisted_enabled() -> Option<bool> {
    let path = config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    doc.get("dashboard")
        .and_then(|d| d.get("enabled"))
        .and_then(|v| v.as_bool())
}

/// Load the full persisted dashboard from `~/.grok/config.toml`. Returns `None` only when the file
/// is missing or completely unreadable. Malformed individual fields fall back to defaults silently.
pub fn load_persisted() -> Option<PersistedDashboard> {
    let path = config_path()?;
    load_persisted_from_path(&path)
}

/// Path-taking variant of [`load_persisted`] so the on-disk round-trip can be exercised in tests against a `tempfile::TempDir`.
/// Falls back to defaults when the table or individual fields are malformed.
pub fn load_persisted_from_path(path: &std::path::Path) -> Option<PersistedDashboard> {
    let content = std::fs::read_to_string(path).ok()?;
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    let table = doc.get("dashboard")?;
    let enabled = table
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let grouping = table
        .get("grouping")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "directory" | "dir" => Some(Grouping::Directory),
            "state" => Some(Grouping::State),
            _ => None,
        })
        .unwrap_or(Grouping::State);
    let pinned: BTreeSet<PersistedRowId> = match table.get("pinned") {
        Some(item) => parse_persist_keys(item),
        None => BTreeSet::new(),
    };
    let reorder: Vec<PersistedRowId> = match table.get("reorder") {
        Some(item) => parse_persist_key_list(item),
        None => Vec::new(),
    };
    Some(PersistedDashboard {
        enabled,
        grouping,
        pinned,
        reorder,
    })
}

/// Synchronous, blocking write of the persisted dashboard config. That helper returns `None` only
/// when the file is non-empty AND unparseable, meaning we. MUST NOT overwrite it (the file may
/// contain user data we cannot interpret).
pub fn write_persisted(p: &PersistedDashboard) -> std::io::Result<()> {
    let path = config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no grok home"))?;
    write_persisted_to_path(&path, p)
}

/// Path-taking variant of [`write_persisted`] so the on-disk round-trip can be exercised in tests against a `tempfile::TempDir`.
pub fn write_persisted_to_path(
    path: &std::path::Path,
    p: &PersistedDashboard,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = match crate::config_toml_edit::read_config_document_for_edit(path) {
        Some(d) => d,
        None => {
            // File exists but is unparseable
            // Refuse to overwrite; doing so would erase every other table
            tracing::warn!(
                path = %path.display(),
                "refusing to persist dashboard: config.toml is non-empty and unparseable"
            );
            return Ok(());
        }
    };
    let dash = doc.entry("dashboard").or_insert(toml_edit::table());
    let Some(t) = dash.as_table_mut() else {
        return Ok(());
    };
    t.insert("enabled", toml_edit::value(p.enabled));
    t.insert(
        "grouping",
        toml_edit::value(match p.grouping {
            Grouping::State => "state",
            Grouping::Directory => "directory",
        }),
    );
    let mut pin_arr = toml_edit::Array::new();
    for id in &p.pinned {
        pin_arr.push(id.to_key());
    }
    t.insert("pinned", toml_edit::value(pin_arr));
    let mut reorder_arr = toml_edit::Array::new();
    for id in &p.reorder {
        reorder_arr.push(id.to_key());
    }
    t.insert("reorder", toml_edit::value(reorder_arr));
    // The onboarding hint was removed; drop the stale table so old configs don't carry a dead `[dashboard.onboarding]` key forever
    t.remove("onboarding");
    atomic_write(path, doc.to_string().as_bytes())
}

/// Atomic write via `<path>.dashboard.tmp.<pid>` then `rename`. Concurrent readers see either the
/// old file or the new file, never a partial truncated copy that would parse as `None` and trigger
/// the catastrophic clobber on the next writer.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let pid = std::process::id();
    let tmp = path.with_extension(format!("toml.dashboard.tmp.{pid}"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // Ensure the bytes are physically on disk before the rename.
        // Without this, the rename can complete and the file system can later observe the renamed inode pointing at zeroed data
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Parent directory fsync (defense in depth on unusual filesystems / network mounts)
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        // sync_all on a directory is allowed on Unix and is a no-op on platforms that don't support it
        // Swallow errors: the rename succeeded, and the durability question is moot if the OS won't fsync the directory
        let _ = dir.sync_all();
    }
    Ok(())
}

fn config_path() -> Option<PathBuf> {
    let home = distill_shell::util::distill_home::distill_home();
    Some(home.join(distill_config::USER_CONFIG_FILENAME))
}

/// Cap parsed entry count to keep a corrupted or malicious config.toml from ballooning allocations.
const MAX_PERSISTED_ENTRIES: usize = 256;
/// Cap persist-key string length so a malformed entry doesn't carry around megabytes.
const MAX_PERSIST_KEY_LEN: usize = 1024;

fn parse_persist_keys(item: &toml_edit::Item) -> BTreeSet<PersistedRowId> {
    let Some(arr) = item.as_array() else {
        tracing::warn!("dashboard.pinned must be an array; ignoring");
        return BTreeSet::new();
    };
    let mut out = BTreeSet::new();
    for v in arr.iter().take(MAX_PERSISTED_ENTRIES) {
        if let Some(s) = v.as_str()
            && s.len() <= MAX_PERSIST_KEY_LEN
            && let Some(id) = PersistedRowId::from_key(s)
        {
            out.insert(id);
        }
    }
    out
}

fn parse_persist_key_list(item: &toml_edit::Item) -> Vec<PersistedRowId> {
    let Some(arr) = item.as_array() else {
        tracing::warn!("dashboard.reorder must be an array; ignoring");
        return Vec::new();
    };
    arr.iter()
        .take(MAX_PERSISTED_ENTRIES)
        .filter_map(|v| {
            v.as_str()
                .filter(|s| s.len() <= MAX_PERSIST_KEY_LEN)
                .and_then(PersistedRowId::from_key)
        })
        .collect()
}

/// Compact a `Path` for display against `$HOME`, returning a `String`. Used by the row renderer and
/// the filter substring search to keep cwd matching consistent. When `cwd == home`, `strip_prefix`
/// returns an empty `Path`; the empty-rest branch collapses to the bare `"~"` rather than `"~/"`.
pub fn compact_cwd(cwd: &Path, home: Option<&str>) -> String {
    if let Some(h) = home
        && let Ok(rest) = cwd.strip_prefix(h)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    cwd.display().to_string()
}

#[cfg(test)]
impl DashboardState {
    /// Test-only: clone the state for tests that can't move ownership.
    /// `PromptWidget` doesn't impl Clone so we cheat by constructing a fresh state with the same selection.
    pub(crate) fn clone_for_test(&self) -> Self {
        let mut s = Self::new();
        s.selected = self.selected.clone();
        s.pinned = self.pinned.clone();
        s.reorder = self.reorder.clone();
        s.grouping = self.grouping;
        s.filter = self.filter.clone();
        s
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
