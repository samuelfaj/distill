// Modified for Distill by Samuel Fajreldines, 2026.
//! Dashboard rows: classification, build, filter, sort.
use super::state::{DashboardRowId, Filter, RowState};
use crate::acp::tracker::TurnActivity;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::roster::{RosterActivity, RosterEntry};
use crate::app::subagent::{SubagentInfo, format_activity_label, format_subagent_label};
use crate::views::dashboard::row_activity::{
    has_live_parent_activity, live_work_badges, top_level_activity, top_level_secondary_line,
};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};
/// Title prefix for a session that has no name / generated title / prompt yet.
/// [`RowTitle::render_wide`](crate::views::dashboard::row_title::RowTitle::render_wide) paints the trailing ` #<id>` suffix dimly.
pub(crate) const NEW_SESSION_LABEL: &str = "New session";
/// A single row in the dashboard. Built per-frame from `app.agents`.
#[derive(Debug, Clone)]
pub struct DashboardRow {
    pub id: DashboardRowId,
    /// Unlike the row id, this stays the same when a saved session is loaded.
    pub session_id: Option<String>,
    /// Display label (e.g. `"implementer · fix login bug"`).
    pub label: String,
    /// Right-of-label subtitle painted after a ` · ` separator in dim text (e.g. `"xai my-branch-2 worktree"`).
    /// `None` when the row has no repo / branch context worth showing.
    pub subtitle: Option<String>,
    /// Coarse state used for grouping.
    pub state: RowState,
    /// Activity summary (e.g. `"Running: cargo test"`). `None` when idle or finished.
    pub activity: Option<String>,
    /// Secondary dim line painted directly below the title row.
    /// Holds the last tool call, the last assistant message, or (for `NeedsInput`) a "Pending: …" preview of the front-most permission request.
    /// `None` collapses the row to a single line (e.g. very recently created with no activity yet).
    pub secondary_line: Option<String>,
    /// Working directory display string (already compacted to `~/...`).
    pub cwd_display: String,
    /// Raw cwd for downstream filter / grouping consumers.
    pub cwd: PathBuf,
    /// Wall-clock moment of the row's last change. A wall-clock [`SystemTime`]: roster timestamps can
    /// predate this process, even the machine's boot, which an [`Instant`] cannot represent.
    pub last_change_at: SystemTime,
    /// True when this row is pinned (always floats above non-pinned).
    pub pinned: bool,
    /// Optional row badges (e.g. `worktree`, `needs-input`).
    pub badges: Vec<RowBadge>,
    /// Context window usage percent, when known. Drives the mini gauge.
    pub context_pct: Option<u8>,
    /// Indent level for nested subagent rendering: 0 for a top-level row, 1 for a subagent under its parent.
    pub indent: u8,
    /// Display title for the parent agent, used to scope group headers in `Grouping::Directory` mode where subagents must sort with their parent.
    pub parent_label: Option<String>,
    /// True when this row is a "… N more" collapse placeholder.
    /// The renderer paints it dimmed and the cursor cannot land on it (selectable=false).
    pub is_more_placeholder: bool,
    /// Number of rolled-up rows when `is_more_placeholder == true`.
    pub more_count: usize,
}
/// Compact badge rendered next to the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowBadge {
    Worktree,
    NeedsInput,
    Subagents(usize),
    Tasks(usize),
    Watchers(usize),
    Workflows(usize),
    Pinned,
    Failed,
}
impl RowBadge {
    pub fn label(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::NeedsInput => "needs-input",
            Self::Subagents(_) => "subagents",
            Self::Tasks(_) => "tasks",
            Self::Watchers(_) => "watchers",
            Self::Workflows(_) => "workflows",
            Self::Pinned => "pinned",
            Self::Failed => "failed",
        }
    }
}
/// Maximum number of subagents to show per parent before collapsing completed/failed ones into a "… N more" row.
pub const MAX_VISIBLE_SUBAGENTS: usize = 8;
/// Process-wide fallback anchor for the age column when an agent lacks both `turn_started_at` and `last_active_at`.
/// Rare: `AgentView::new` always stamps the latter, but a future caller could omit it.
/// Initialised lazily on first call so the fallback stays frozen instead of re-anchoring every frame.
fn fallback_epoch() -> Instant {
    static FALLBACK: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *FALLBACK.get_or_init(Instant::now)
}
/// Iterates `agents` so a stale `AgentId` can never panic. Subagents are appended directly after
/// their parent, never reordered across parents.
pub fn build_rows(
    agents: &IndexMap<AgentId, AgentView>,
    pinned: &std::collections::BTreeSet<DashboardRowId>,
    reorder: &[DashboardRowId],
    grouping: super::state::Grouping,
    filter: &Filter,
    home: Option<&str>,
) -> Vec<DashboardRow> {
    let mut rows = build_local_rows(agents, pinned, home, true);
    apply_filter(&mut rows, filter, home);
    sort_rows(&mut rows, grouping, reorder);
    rows
}
/// It appends "roster-only" rows for leader sessions this client is not locally attached to (the
/// FleetView dashboard). It does not list subagents as their own rows; only top-level agents and
/// roster sessions appear (see [`build_local_rows`]).
pub fn build_rows_with_roster(
    agents: &IndexMap<AgentId, AgentView>,
    pinned: &std::collections::BTreeSet<DashboardRowId>,
    reorder: &[DashboardRowId],
    grouping: super::state::Grouping,
    filter: &Filter,
    home: Option<&str>,
    roster: &[RosterEntry],
) -> Vec<DashboardRow> {
    let mut rows = build_local_rows(agents, pinned, home, false);
    append_roster_rows(&mut rows, roster, agents, pinned, home);
    apply_filter(&mut rows, filter, home);
    sort_rows(&mut rows, grouping, reorder);
    rows
}
pub(crate) fn workspace_member_is_pinned(member: &distill_dashboard_store::Member) -> bool {
    member.pin_rank.is_some()
}
#[derive(Clone, Copy, Default)]
pub(crate) struct WorkspaceRowInputs<'a> {
    /// `None` until the store snapshot has loaded, or when the store failed to open.
    pub workspace: Option<&'a crate::app::workspace_layout::WorkspaceView>,
    /// Live top-level agents with no committed member yet; `crate::app::workspace_sync` decides which qualify.
    pub provisional: &'a [AgentId],
}
impl WorkspaceRowInputs<'_> {
    pub fn grouping(&self) -> super::state::Grouping {
        self.workspace
            .map_or(super::state::Grouping::State, |view| view.grouping.into())
    }
}
/// The store is authoritative for membership, layout, and sessions this process has not loaded.
pub(crate) fn build_rows_with_workspace(
    agents: &IndexMap<AgentId, AgentView>,
    inputs: WorkspaceRowInputs<'_>,
    filter: &Filter,
    home: Option<&str>,
) -> Vec<DashboardRow> {
    let live_by_session: HashMap<&str, (AgentId, &AgentView)> = agents
        .iter()
        .filter_map(|(id, agent)| {
            agent
                .session
                .session_id
                .as_ref()
                .map(|session_id| (session_id.0.as_ref(), (*id, agent)))
        })
        .collect();
    let reorder = inputs
        .workspace
        .map(|workspace| workspace.manual_order())
        .unwrap_or_default()
        .into_iter()
        .map(|key| {
            live_by_session.get(key.session_id.as_ref()).map_or_else(
                || DashboardRowId::Workspace {
                    session_id: key.session_id.to_string(),
                },
                |(id, _)| DashboardRowId::TopLevel(*id),
            )
        })
        .collect::<Vec<_>>();
    let mut rows = inputs
        .workspace
        .into_iter()
        .flat_map(|workspace| workspace.members.iter())
        .filter(|member| matches!(member.kind, distill_dashboard_store::MemberKind::Build))
        .filter_map(|member| {
            let live = live_by_session.get(member.session_id.as_ref()).copied();
            let is_pinned = workspace_member_is_pinned(member);
            if !workspace_member_is_visible(member, live.map(|(_, agent)| agent), is_pinned) {
                return None;
            }
            if let Some((id, agent)) = live {
                return Some(top_level_row(id, agent, is_pinned, home));
            }
            Some(workspace_member_row(member, is_pinned, home))
        })
        .collect::<Vec<_>>();
    rows.extend(inputs.provisional.iter().filter_map(|id| {
        let agent = agents.get(id)?;
        if is_empty_idle_top_level(agent) {
            return None;
        }
        Some(top_level_row(*id, agent, false, home))
    }));
    apply_filter(&mut rows, filter, home);
    sort_rows(&mut rows, inputs.grouping(), &reorder);
    rows
}
pub(crate) fn workspace_member_is_visible(
    member: &distill_dashboard_store::Member,
    live_agent: Option<&AgentView>,
    pinned: bool,
) -> bool {
    if !matches!(member.kind, distill_dashboard_store::MemberKind::Build) {
        return false;
    }
    if pinned {
        return true;
    }
    match live_agent {
        Some(agent) => !is_empty_idle_top_level(agent),
        None => workspace_member_has_content(member),
    }
}
fn workspace_member_has_content(member: &distill_dashboard_store::Member) -> bool {
    let has_text = |value: Option<&str>| value.is_some_and(|text| !text.trim().is_empty());
    has_text(member.title.as_deref()) || has_text(member.last_turn_summary.as_deref())
}
fn workspace_member_row(
    member: &distill_dashboard_store::Member,
    pinned: bool,
    home: Option<&str>,
) -> DashboardRow {
    let session_id = member.session_id.as_ref();
    let cwd = member
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default();
    let label = member
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(sanitize)
        .or_else(|| {
            cwd.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(sanitize)
        })
        .unwrap_or_else(|| sanitize(session_id));
    let mut badges = Vec::new();
    if member.is_worktree {
        badges.push(RowBadge::Worktree);
    }
    if pinned {
        badges.push(RowBadge::Pinned);
    }
    DashboardRow {
        id: DashboardRowId::Workspace {
            session_id: session_id.to_owned(),
        },
        session_id: Some(session_id.to_owned()),
        label,
        subtitle: None,
        state: RowState::Idle,
        activity: None,
        secondary_line: member
            .last_turn_summary
            .as_deref()
            .or(member.model.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(sanitize),
        cwd_display: if cwd.as_os_str().is_empty() {
            String::new()
        } else {
            super::state::compact_cwd(&cwd, home)
        },
        cwd,
        last_change_at: crate::util::system_time_from_unix_ms(member.last_change_unix_ms),
        pinned,
        badges,
        context_pct: None,
        indent: 0,
        parent_label: None,
        is_more_placeholder: false,
        more_count: 0,
    }
}
/// Build the local-agent rows WITHOUT applying filter or sort. Shared by [`build_rows`] and
/// [`build_rows_with_roster`].
fn build_local_rows(
    agents: &IndexMap<AgentId, AgentView>,
    pinned: &std::collections::BTreeSet<DashboardRowId>,
    home: Option<&str>,
    include_subagents: bool,
) -> Vec<DashboardRow> {
    let mut rows = Vec::new();
    for (id, agent) in agents.iter() {
        let top_id = DashboardRowId::TopLevel(*id);
        if is_empty_idle_top_level(agent) && !pinned.contains(&top_id) {
            continue;
        }
        let row = top_level_row(*id, agent, pinned.contains(&top_id), home);
        rows.push(row);
        if !include_subagents {
            continue;
        }
        let mut subagents: Vec<&SubagentInfo> = agent
            .subagent_sessions
            .values()
            .filter(|info| info.attempt.workflow_run_id.is_none())
            .collect();
        subagents.sort_by(|a, b| {
            let a_running = a.is_running();
            let b_running = b.is_running();
            match (a_running, b_running) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.attempt.started_at.cmp(&a.attempt.started_at),
            }
        });
        let total = subagents.len();
        let keep = MAX_VISIBLE_SUBAGENTS.min(total);
        for info in subagents.iter().take(keep) {
            rows.push(subagent_row(
                *id,
                agent,
                info,
                pinned.contains(&DashboardRowId::Subagent {
                    parent: *id,
                    child_session_id: info.child_session_id.to_string(),
                }),
                home,
            ));
        }
        if total > keep {
            let agent_label = top_level_label(agent);
            rows.push(DashboardRow {
                id: DashboardRowId::Subagent {
                    parent: *id,
                    child_session_id: format!("__more_{}", id.0),
                },
                session_id: None,
                label: format!("\u{2026} {} more", total - keep),
                subtitle: None,
                state: RowState::Idle,
                activity: None,
                secondary_line: None,
                cwd_display: String::new(),
                cwd: agent.session.cwd.clone(),
                last_change_at: SystemTime::now(),
                pinned: false,
                badges: Vec::new(),
                context_pct: None,
                indent: 1,
                parent_label: Some(agent_label),
                is_more_placeholder: true,
                more_count: total - keep,
            });
        }
    }
    rows
}
/// Map a leader [`RosterActivity`] to the dashboard's coarse [`RowState`].
/// Public so the dispatcher can gate roster-row deletion through the same `RowState::allows_delete` predicate the renderer paints `[✗]` with.
pub fn roster_activity_to_state(activity: RosterActivity) -> RowState {
    match activity {
        RosterActivity::Working => RowState::Working,
        RosterActivity::NeedsInput => RowState::NeedsInput,
        RosterActivity::Idle | RosterActivity::Dormant => RowState::Inactive,
        RosterActivity::Completed => RowState::Completed,
        RosterActivity::Dead => RowState::Failed,
    }
}
/// Append "roster-only" rows for leader sessions not represented by a local `AgentView`.
/// Skips any roster entry whose `session_id` already matches a locally-attached agent (those carry richer data via [`build_local_rows`]).
fn append_roster_rows(
    rows: &mut Vec<DashboardRow>,
    roster: &[RosterEntry],
    agents: &IndexMap<AgentId, AgentView>,
    pinned: &std::collections::BTreeSet<DashboardRowId>,
    home: Option<&str>,
) {
    if roster.is_empty() {
        return;
    }
    let local_ids: std::collections::HashSet<&str> = agents
        .values()
        .filter_map(|a| a.session.session_id.as_ref().map(|s| s.0.as_ref()))
        .collect();
    for entry in roster {
        if local_ids.contains(entry.session_id.as_str()) {
            continue;
        }
        let id = DashboardRowId::Roster {
            session_id: entry.session_id.clone(),
        };
        let has_title = entry
            .title
            .as_deref()
            .map(str::trim)
            .is_some_and(|t| !t.is_empty());
        let active = matches!(
            entry.activity,
            RosterActivity::Working | RosterActivity::NeedsInput
        );
        if !has_title && !active && !pinned.contains(&id) {
            continue;
        }
        let cwd = PathBuf::from(&entry.cwd);
        let label = entry
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sanitize)
            .or_else(|| {
                cwd.file_name()
                    .and_then(|n| n.to_str())
                    .filter(|s| !s.is_empty())
                    .map(sanitize)
            })
            .unwrap_or_else(|| sanitize(&entry.session_id));
        let state = roster_activity_to_state(entry.activity);
        let activity = match state {
            RowState::NeedsInput => Some("Awaiting input".to_string()),
            RowState::Working => Some("Working".to_string()),
            _ => None,
        };
        let mut badges = Vec::new();
        if entry.is_worktree {
            badges.push(RowBadge::Worktree);
        }
        if entry.activity == RosterActivity::NeedsInput {
            badges.push(RowBadge::NeedsInput);
        }
        let cwd_display = super::state::compact_cwd(&cwd, home);
        let is_pinned = pinned.contains(&id);
        rows.push(DashboardRow {
            id,
            session_id: Some(entry.session_id.clone()),
            label,
            subtitle: None,
            state,
            activity,
            secondary_line: entry
                .last_turn_summary
                .as_deref()
                .or(entry.model_id.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(sanitize),
            cwd_display,
            cwd,
            last_change_at: crate::util::system_time_from_unix_ms(entry.last_change_unix_ms),
            pinned: is_pinned,
            badges,
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        });
    }
}
/// Classify a top-level agent. A row in `AgentState::CommandRunning { Compact, .. }` is therefore
/// "Working" and its activity label reads `Compacting`. Each is ongoing, user-dispatched work, and
/// monitors and loops can wake the agent for a fresh turn, so the agent isn't meaningfully idle.
pub fn classify_top_level(agent: &AgentView) -> RowState {
    if !agent.permission_queue.is_empty() || agent.question_view.is_some() {
        return RowState::NeedsInput;
    }
    if has_live_parent_activity(agent) {
        return RowState::Working;
    }
    if has_background_work(agent) {
        return RowState::Working;
    }
    RowState::Idle
}
/// Whether `agent` has live background work that keeps it out of the `Idle` group even when its
/// turn is idle.
pub fn has_background_work(agent: &AgentView) -> bool {
    agent
        .session
        .bg_tasks
        .values()
        .any(|t| t.status == crate::app::agent::BgTaskStatus::Running)
        || !agent.session.scheduled_tasks.is_empty()
}
/// Classify a subagent.
///
/// Subagents never enter `NeedsInput` in v1; they have no way to ask the user for input. A test asserts this.
pub fn classify_subagent(info: &SubagentInfo) -> RowState {
    if info.is_finished() {
        match info.attempt.status.as_deref() {
            Some("failed") | Some("cancelled") | Some("error") => RowState::Failed,
            _ => RowState::Completed,
        }
    } else {
        RowState::Working
    }
}
/// Sanitise every string derived from backend / model-controlled content before returning.
/// The dashboard's renderer paints raw via `set_string`, which preserves embedded escape sequences in the ratatui buffer.
pub(crate) fn sanitize(s: &str) -> String {
    crate::views::session_title::sanitize_display_text(s).into_owned()
}
/// Whether a top-level agent has no real conversation yet. These are the exact conditions under
/// which [`top_level_label`] falls back to `New session #<id>`. A session created on pager launch
/// carries only the system prompt and injected `<system-reminder>` context.
fn is_empty_top_level(agent: &AgentView) -> bool {
    let has_text = |s: Option<&str>| s.map(str::trim).is_some_and(|t| !t.is_empty());
    if has_text(agent.display_name.as_deref()) {
        return false;
    }
    if has_text(agent.generated_session_title.as_deref()) {
        return false;
    }
    if !agent.session.pending_prompts.is_empty() {
        return false;
    }
    if !agent.subagent_sessions.is_empty() || !agent.session.bg_tasks.is_empty() {
        return false;
    }
    if super::peek::extract_first_user_message(agent).is_some() {
        return false;
    }
    true
}
pub(crate) fn is_empty_idle_top_level(agent: &AgentView) -> bool {
    is_empty_top_level(agent)
        && !matches!(
            classify_top_level(agent),
            RowState::Working | RowState::NeedsInput
        )
}
fn top_level_label(agent: &AgentView) -> String {
    if let Some(name) = agent.display_name.as_deref() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return sanitize(trimmed);
        }
    }
    if let Some(title) = agent.generated_session_title.as_deref() {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return sanitize(trimmed);
        }
    }
    if let Some(first) = agent.session.pending_prompts.front() {
        let preview: String = first.text.trim().chars().take(30).collect();
        if !preview.is_empty() {
            return sanitize(&preview);
        }
    }
    if let Some(first_msg) = super::peek::extract_first_user_message(agent) {
        let preview: String = first_msg.chars().take(30).collect();
        if !preview.is_empty() {
            return preview;
        }
    }
    if let Some(sid) = agent.session.session_id.as_ref() {
        let short: String = sid.0.chars().take(8).collect();
        return format!("{NEW_SESSION_LABEL} #{short}");
    }
    NEW_SESSION_LABEL.to_string()
}
fn top_level_row(id: AgentId, agent: &AgentView, pinned: bool, home: Option<&str>) -> DashboardRow {
    let state = classify_top_level(agent);
    let label = top_level_label(agent);
    let subtitle = top_level_subtitle(agent);
    let activity = top_level_activity(agent, state);
    let secondary_line = top_level_secondary_line(agent, state, activity.as_deref());
    let last_change_at = top_level_last_change_at(agent, state);
    let mut badges = Vec::new();
    if agent.is_worktree {
        badges.push(RowBadge::Worktree);
    }
    if state == RowState::NeedsInput {
        badges.push(RowBadge::NeedsInput);
    }
    badges.extend(live_work_badges(agent));
    if pinned {
        badges.push(RowBadge::Pinned);
    }
    let cwd_display = super::state::compact_cwd(&agent.session.cwd, home);
    DashboardRow {
        id: DashboardRowId::TopLevel(id),
        session_id: agent.session.session_id.as_ref().map(|id| id.0.to_string()),
        label,
        subtitle,
        state,
        activity,
        secondary_line,
        cwd_display,
        cwd: agent.session.cwd.clone(),
        last_change_at,
        pinned,
        badges,
        context_pct: agent.context_state.as_ref().map(|c| c.usage_pct),
        indent: 0,
        parent_label: None,
        is_more_placeholder: false,
        more_count: 0,
    }
}
/// Wall-clock anchor used by both dashboard rows and workspace metadata sync.
pub(crate) fn top_level_last_change_at(agent: &AgentView, state: RowState) -> SystemTime {
    let anchor: Instant = match state {
        RowState::Working => agent
            .turn_started_at
            .or(agent.last_active_at)
            .unwrap_or_else(fallback_epoch),
        RowState::NeedsInput
        | RowState::Idle
        | RowState::Inactive
        | RowState::Completed
        | RowState::Failed => agent.last_active_at.unwrap_or_else(fallback_epoch),
    };
    crate::util::system_time_from_instant(anchor)
}
fn subagent_row(
    parent: AgentId,
    parent_view: &AgentView,
    info: &SubagentInfo,
    pinned: bool,
    home: Option<&str>,
) -> DashboardRow {
    let state = classify_subagent(info);
    let (label_raw, desc_raw) = format_subagent_label(info);
    let label = {
        let label = sanitize(&label_raw);
        let desc = sanitize(&desc_raw);
        if desc.trim().is_empty() {
            label
        } else {
            format!("{label} · {desc}")
        }
    };
    let activity = subagent_activity(info, state);
    let cwd = info
        .child_cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| parent_view.session.cwd.clone());
    let cwd_display = super::state::compact_cwd(&cwd, home);
    let last_change_at = crate::util::system_time_from_instant(info.attempt.last_progress_at);
    let mut badges = Vec::new();
    if info.worktree_path.is_some() {
        badges.push(RowBadge::Worktree);
    }
    if pinned {
        badges.push(RowBadge::Pinned);
    }
    if state == RowState::Failed {
        badges.push(RowBadge::Failed);
    }
    let parent_label = Some(top_level_label(parent_view));
    let subtitle = subagent_subtitle(info, &cwd);
    let secondary_line = subagent_secondary_line(info, state, activity.as_deref());
    DashboardRow {
        id: DashboardRowId::Subagent {
            parent,
            child_session_id: info.child_session_id.to_string(),
        },
        session_id: Some(info.child_session_id.to_string()),
        label,
        subtitle,
        state,
        activity,
        secondary_line,
        cwd_display,
        cwd,
        last_change_at,
        pinned,
        badges,
        context_pct: info.attempt.context_usage_pct,
        indent: 1,
        parent_label,
        is_more_placeholder: false,
        more_count: 0,
    }
}
/// Basename of a directory (the leaf folder name), or `None` for the filesystem root / an empty path.
fn cwd_basename(cwd: &std::path::Path) -> Option<String> {
    cwd.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}
/// The branch shown is therefore the latest, not a stale notification value.
fn top_level_subtitle(agent: &AgentView) -> Option<String> {
    let lazy = crate::git_info::cwd_git_info_lazy(&agent.session.cwd);
    let trimmed = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let branch = agent
        .current_branch
        .as_deref()
        .and_then(trimmed)
        .or_else(|| {
            lazy.as_ref()
                .and_then(|i| i.branch.as_deref())
                .and_then(trimmed)
        });
    let is_worktree = agent.is_worktree || lazy.as_ref().is_some_and(|i| i.is_worktree);
    let location = if is_worktree {
        agent
            .worktree_label
            .as_deref()
            .and_then(trimmed)
            .or_else(|| {
                lazy.as_ref()
                    .and_then(|i| i.worktree_label.as_deref())
                    .and_then(trimmed)
            })
            .or_else(|| cwd_basename(&agent.session.cwd))
    } else {
        cwd_basename(&agent.session.cwd)
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = branch {
        parts.push(sanitize(&b));
    }
    if let Some(loc) = location {
        parts.push(sanitize(&loc));
    }
    if parts.is_empty() {
        return None;
    }
    if is_worktree {
        parts.push("worktree".to_string());
    }
    Some(parts.join(" "))
}
/// Subagent equivalent of `top_level_subtitle`.
/// Subagents don't carry their own branch / repo metadata, so we show the cwd's folder name plus a `worktree` suffix when one is set.
/// In a worktree the cwd's folder name is the worktree folder name.
fn subagent_subtitle(info: &SubagentInfo, cwd: &std::path::Path) -> Option<String> {
    let name = cwd_basename(cwd)?;
    if info.worktree_path.is_some() {
        Some(format!("{name} worktree"))
    } else {
        Some(name)
    }
}
/// Secondary line for subagent rows: the running tool (working) or the duration summary (finished).
fn subagent_secondary_line(
    _info: &SubagentInfo,
    _state: RowState,
    activity: Option<&str>,
) -> Option<String> {
    activity.map(sanitize)
}
fn subagent_activity(info: &SubagentInfo, state: RowState) -> Option<String> {
    if state == RowState::Working {
        if let Some(label) = info
            .attempt
            .activity_label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(sanitize(label));
        }
        let last_tool = info
            .attempt
            .tools_used
            .last()
            .map(|s| s.as_ref())
            .unwrap_or("");
        if last_tool.is_empty() {
            Some("Working".to_string())
        } else {
            Some(sanitize(&format_activity_label(
                &TurnActivity::ToolRunning {
                    title: last_tool.to_string(),
                    description: None,
                },
            )))
        }
    } else if info.is_finished() {
        let turns = info.attempt.turns.unwrap_or(0);
        let tools = info.attempt.tool_calls.unwrap_or(0);
        let toks = info.attempt.tokens_used.unwrap_or(0);
        Some(format!("{tools} tools · {toks} tok · {turns} turns"))
    } else {
        None
    }
}
/// Apply a filter to the list in place.
///
/// Needle is lowercased once outside `retain` instead of per-row, so a 100-row list doesn't allocate 100 fresh lowercase Strings per keystroke.
pub fn apply_filter(rows: &mut Vec<DashboardRow>, filter: &Filter, _home: Option<&str>) {
    if matches!(filter, Filter::None) {
        return;
    }
    let needle_lower: Option<String> = match filter {
        Filter::Agent(s) | Filter::Substring(s) => Some(s.to_lowercase()),
        _ => None,
    };
    rows.retain(|r| {
        if r.is_more_placeholder {
            return true;
        }
        match filter {
            Filter::None => true,
            Filter::Agent(_) => {
                let n = needle_lower.as_deref().unwrap_or("");
                r.label.to_lowercase().contains(n)
                    || r.parent_label
                        .as_deref()
                        .is_some_and(|p| p.to_lowercase().contains(n))
            }
            Filter::State(rs) => r.state == *rs,
            Filter::Substring(_) => {
                let n = needle_lower.as_deref().unwrap_or("");
                if n.is_empty() {
                    return true;
                }
                r.label.to_lowercase().contains(n) || r.cwd_display.to_lowercase().contains(n)
            }
        }
    });
}
/// Pinned rows keep manual order across activity and grouping changes.
/// Unpinned rows reorder only within their state or directory group.
pub fn sort_rows(
    rows: &mut [DashboardRow],
    grouping: super::state::Grouping,
    reorder: &[DashboardRowId],
) {
    use super::state::Grouping;
    match grouping {
        Grouping::State => sort_within_groups(rows, reorder),
        Grouping::Directory => sort_within_directory_groups(rows, reorder),
    }
}
fn sort_within_groups(rows: &mut [DashboardRow], reorder: &[DashboardRowId]) {
    let clusters = build_clusters(rows);
    let snapshot = rows.to_vec();
    let mut indexed: Vec<(usize, usize, ClusterKey<'_>)> = clusters
        .iter()
        .filter_map(|(start, end)| {
            let parent = snapshot.get(*start)?;
            Some((
                *start,
                *end,
                ClusterKey {
                    pinned: parent.pinned,
                    state: parent.state.group_priority(),
                    last_change_at: parent.last_change_at,
                    reorder_idx: reorder.iter().position(|r| *r == parent.id),
                    session_id: parent.session_id.as_deref(),
                    id: parent.id.clone(),
                },
            ))
        })
        .collect();
    indexed.sort_by(|(_, _, left), (_, _, right)| sort_cluster_key(left, right, true));
    let mut write = 0usize;
    for (start, end, _) in &indexed {
        for src in snapshot.iter().take(*end).skip(*start) {
            if let Some(slot) = rows.get_mut(write) {
                *slot = src.clone();
                write += 1;
            }
        }
    }
}
fn sort_within_directory_groups(rows: &mut [DashboardRow], reorder: &[DashboardRowId]) {
    let clusters = build_clusters(rows);
    let snapshot = rows.to_vec();
    let mut indexed: Vec<(usize, usize, (String, ClusterKey<'_>))> = clusters
        .iter()
        .filter_map(|(start, end)| {
            let parent = snapshot.get(*start)?;
            let key = ClusterKey {
                pinned: parent.pinned,
                state: parent.state.group_priority(),
                last_change_at: parent.last_change_at,
                reorder_idx: reorder.iter().position(|r| *r == parent.id),
                session_id: parent.session_id.as_deref(),
                id: parent.id.clone(),
            };
            Some((*start, *end, (parent.cwd_display.clone(), key)))
        })
        .collect();
    indexed.sort_by(|(_, _, (left_cwd, left)), (_, _, (right_cwd, right))| {
        if left.pinned || right.pinned {
            return sort_cluster_key(left, right, false);
        }
        left_cwd
            .cmp(right_cwd)
            .then_with(|| sort_cluster_key(left, right, false))
    });
    let mut write = 0usize;
    for (start, end, _) in &indexed {
        for src in snapshot.iter().take(*end).skip(*start) {
            if let Some(slot) = rows.get_mut(write) {
                *slot = src.clone();
                write += 1;
            }
        }
    }
}
#[derive(Debug, Clone)]
struct ClusterKey<'a> {
    pinned: bool,
    state: u8,
    last_change_at: SystemTime,
    reorder_idx: Option<usize>,
    session_id: Option<&'a str>,
    /// Final tiebreak: when every other field is equal, fall back to the parent row's id so the order is deterministic across rebuilds.
    /// This avoids relying on `sort_by`'s stable-on-equal pass-through.
    id: DashboardRowId,
}
/// `state_before_reorder` restricts manual ordering of unpinned rows to the same state.
fn sort_cluster_key(
    a: &ClusterKey<'_>,
    b: &ClusterKey<'_>,
    state_before_reorder: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a.pinned != b.pinned {
        return b.pinned.cmp(&a.pinned);
    }
    let by_reorder = |a: &ClusterKey<'_>, b: &ClusterKey<'_>| match (a.reorder_idx, b.reorder_idx) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    if a.pinned {
        return by_reorder(a, b)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.id.cmp(&b.id));
    }
    let by_state = |a: &ClusterKey<'_>, b: &ClusterKey<'_>| b.state.cmp(&a.state);
    if state_before_reorder {
        if a.state != b.state {
            return by_state(a, b);
        }
        match by_reorder(a, b) {
            Ordering::Equal => {}
            ord => return ord,
        }
    } else {
        match by_reorder(a, b) {
            Ordering::Equal => {}
            ord => return ord,
        }
        if a.state != b.state {
            return by_state(a, b);
        }
    }
    b.last_change_at
        .cmp(&a.last_change_at)
        .then_with(|| a.id.cmp(&b.id))
}
/// Identify `(start, end)` ranges of clustered rows: each cluster is one top-level row followed by zero or more subagents.
fn build_clusters(rows: &[DashboardRow]) -> Vec<(usize, usize)> {
    let mut clusters = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let start = i;
        i += 1;
        while i < rows.len() && rows.get(i).is_some_and(|r| r.indent > 0) {
            i += 1;
        }
        clusters.push((start, i));
    }
    clusters
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent_test_fixtures::{running_bg_task, scheduled_loop};
    use crate::app::workspace_test_fixtures::snapshot as workspace_snapshot;
    use agent_client_protocol as acp;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};
    fn nth(rows: &[DashboardRow], i: usize) -> &DashboardRow {
        rows.get(i)
            .unwrap_or_else(|| panic!("expected row {i}, got {rows:?}"))
    }
    /// Row tests render the model and summary and sort by a realistic timestamp.
    fn workspace_member(
        session_id: &str,
        title: &str,
        summary: Option<&str>,
    ) -> distill_dashboard_store::Member {
        let mut member = crate::app::workspace_test_fixtures::member(session_id, title);
        member.model = Some("grok-test".to_owned());
        member.last_turn_summary = summary.map(str::to_owned);
        member.last_change_unix_ms = 1_725_000_000_000;
        member
    }
    fn workspace_rows(
        agents: &IndexMap<AgentId, AgentView>,
        snapshot: &distill_dashboard_store::WorkspaceSnapshot,
    ) -> Vec<DashboardRow> {
        workspace_rows_with_provisional(agents, Some(snapshot), &[])
    }
    fn workspace_rows_with_provisional(
        agents: &IndexMap<AgentId, AgentView>,
        snapshot: Option<&distill_dashboard_store::WorkspaceSnapshot>,
        provisional: &[AgentId],
    ) -> Vec<DashboardRow> {
        let workspace = snapshot.map(crate::app::workspace_layout::WorkspaceView::from_snapshot);
        build_rows_with_workspace(
            agents,
            WorkspaceRowInputs {
                workspace: workspace.as_ref(),
                provisional,
            },
            &Filter::None,
            None,
        )
    }
    #[test]
    fn live_match_enriches_member_row_and_non_provisional_live_agents_stay_hidden() {
        let mut matched = crate::app::agent_view::test_fixtures::make_agent();
        matched.session.session_id = Some(acp::SessionId::new("saved"));
        matched.display_name = Some("Live title".to_owned());
        let mut unadopted = crate::app::agent_view::test_fixtures::make_agent();
        unadopted.session.session_id = Some(acp::SessionId::new("not-saved"));
        unadopted.display_name = Some("Hidden title".to_owned());
        let agents = IndexMap::from([(AgentId(7), matched), (AgentId(8), unadopted)]);
        let snapshot = workspace_snapshot(vec![workspace_member(
            "saved",
            "Stored title",
            Some("Stored summary"),
        )]);
        let rows = workspace_rows(&agents, &snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(nth(&rows, 0).id, DashboardRowId::TopLevel(AgentId(7)));
        assert_eq!(nth(&rows, 0).label, "Live title");
    }
    #[test]
    fn provisional_dispatch_row_shows_before_session_id_binds() {
        let mut dispatched = crate::app::agent_view::test_fixtures::make_agent();
        dispatched
            .session
            .enqueue_prompt("fix the login bug before lunch".into());
        assert!(dispatched.session.session_id.is_none());
        let agents = IndexMap::from([(AgentId(3), dispatched)]);
        let snapshot = workspace_snapshot(vec![workspace_member("other", "Other", None)]);
        let rows = workspace_rows_with_provisional(&agents, Some(&snapshot), &[AgentId(3)]);
        assert_eq!(rows.len(), 2);
        let row = &nth(&rows, 0);
        assert_eq!(row.id, DashboardRowId::TopLevel(AgentId(3)));
        assert_eq!(row.state, RowState::Working);
        assert_eq!(row.label, "fix the login bug before lunch");
        assert!(!row.pinned);
    }
    #[test]
    fn provisional_worktree_row_reports_creation_activity() {
        let mut creating = crate::app::agent_view::test_fixtures::make_agent();
        creating
            .session
            .start_command(crate::app::agent::AgentCommand::CreateWorktree);
        creating.session.enqueue_prompt("queued".into());
        let agents = IndexMap::from([(AgentId(4), creating)]);
        let rows = workspace_rows_with_provisional(&agents, None, &[AgentId(4)]);
        assert_eq!(rows.len(), 1, "live rows render before the snapshot loads");
        assert_eq!(nth(&rows, 0).state, RowState::Working);
        assert_eq!(
            nth(&rows, 0).activity.as_deref(),
            Some("Creating worktree…")
        );
    }
    #[test]
    fn empty_idle_provisional_agent_stays_hidden() {
        let agents = IndexMap::from([(
            AgentId(6),
            crate::app::agent_view::test_fixtures::make_agent(),
        )]);
        let rows = workspace_rows_with_provisional(&agents, None, &[AgentId(6)]);
        assert!(rows.is_empty());
    }
    #[test]
    fn workspace_rows_sort_working_first_then_by_descending_time() {
        let mut working = crate::app::agent_view::test_fixtures::make_agent();
        working.session.session_id = Some(acp::SessionId::new("working"));
        working.session.enqueue_prompt("queued work".into());
        let agents = IndexMap::from([(AgentId(7), working)]);
        let mut old_idle = workspace_member("old-idle", "Old idle", None);
        old_idle.last_change_unix_ms = 1_000;
        let mut fresh_idle = workspace_member("fresh-idle", "Fresh idle", None);
        fresh_idle.last_change_unix_ms = 3_000;
        let mut working_member = workspace_member("working", "Stored working", None);
        working_member.last_change_unix_ms = 500;
        let snapshot = workspace_snapshot(vec![old_idle, working_member, fresh_idle]);
        let rows = workspace_rows(&agents, &snapshot);
        assert_eq!(rows.len(), 3);
        assert_eq!(nth(&rows, 0).id, DashboardRowId::TopLevel(AgentId(7)));
        assert_eq!(nth(&rows, 0).state, RowState::Working);
        assert_eq!(
            nth(&rows, 1).id,
            DashboardRowId::Workspace {
                session_id: "fresh-idle".into()
            }
        );
        assert_eq!(
            nth(&rows, 2).id,
            DashboardRowId::Workspace {
                session_id: "old-idle".into()
            }
        );
    }
    #[test]
    fn workspace_rows_ignore_non_build_members() {
        let mut conversation = workspace_member("shared", "Conversation", None);
        conversation.kind = distill_dashboard_store::MemberKind::Conversation;
        let snapshot = workspace_snapshot(vec![
            workspace_member("shared", "Build", None),
            conversation,
        ]);
        let rows = workspace_rows(&IndexMap::new(), &snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            nth(&rows, 0).id,
            DashboardRowId::Workspace {
                session_id: "shared".to_owned()
            }
        );
        assert_eq!(nth(&rows, 0).label, "Build");
    }
    #[test]
    fn workspace_rows_hide_empty_startup_members_like_v1() {
        let mut stale = workspace_member("empty", "", None);
        stale.title = None;
        assert_eq!(stale.model.as_deref(), Some("grok-test"));
        let snapshot = workspace_snapshot(vec![stale]);
        let rows = workspace_rows(&IndexMap::new(), &snapshot);
        assert!(
            rows.is_empty(),
            "an unloaded startup placeholder must not resurface as a cwd row"
        );
    }
    #[test]
    fn workspace_rows_hide_matching_empty_idle_live_agent() {
        let mut member = workspace_member("empty-live", "", None);
        member.title = None;
        let snapshot = workspace_snapshot(vec![member]);
        let mut empty = crate::app::agent_view::test_fixtures::make_agent();
        empty.session.session_id = Some(acp::SessionId::new("empty-live"));
        let agents = IndexMap::from([(AgentId(9), empty)]);
        let rows = workspace_rows(&agents, &snapshot);
        assert!(
            rows.is_empty(),
            "v2 must apply v1's empty-idle suppression to live matches"
        );
    }
    #[test]
    fn unloaded_workspace_members_render_idle_metadata() {
        let mut second = workspace_member("second", "Second", None);
        second.is_worktree = true;
        let snapshot = workspace_snapshot(vec![
            workspace_member("first", "First", Some("First summary")),
            second,
        ]);
        let rows = workspace_rows(&IndexMap::new(), &snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            nth(&rows, 0).id,
            DashboardRowId::Workspace {
                session_id: "first".to_owned()
            }
        );
        assert_eq!(nth(&rows, 0).state, RowState::Idle);
        assert_eq!(
            nth(&rows, 0).secondary_line.as_deref(),
            Some("First summary")
        );
        assert_eq!(
            nth(&rows, 1).id,
            DashboardRowId::Workspace {
                session_id: "second".to_owned()
            }
        );
        assert_eq!(nth(&rows, 1).secondary_line.as_deref(), Some("grok-test"));
        assert!(nth(&rows, 1).badges.contains(&RowBadge::Worktree));
    }
    fn make_subagent(child_id: &str, finished: bool, status: Option<&str>) -> SubagentInfo {
        let now = Instant::now();
        let mut info = SubagentInfo {
            subagent_id: Arc::from(format!("sa-{child_id}")),
            child_session_id: Arc::from(child_id),
            description: Arc::from("test task"),
            subagent_type: Arc::from("explore"),
            attempt: crate::app::subagent::SubagentAttemptInfo {
                lifecycle: crate::app::subagent::SubagentLifecycleState::running_legacy_for_test(),
                persona: None,
                role: None,
                model: None,
                context_source: None,
                resumed_from: None,
                capability_mode: None,
                workflow_run_id: None,
                context_normalized: false,
                parent_prompt_id: None,
                started_at: now,
                last_progress_at: now,
                status: status.map(Arc::from),
                error: None,
                duration_ms: None,
                tool_calls: None,
                turns: None,
                turn_count: None,
                tool_call_count: None,
                tokens_used: None,
                context_window_tokens: None,
                context_usage_pct: None,
                tools_used: Vec::new(),
                error_count: None,
                activity_label: None,
                is_background: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                terminal_entry_id: None,
            },
            completed_attempt_tokens: 0,
            sealed_attempt_tokens: Default::default(),
            prompt: None,
            child_cwd: None,
            worktree_path: None,
            transcript: Default::default(),
        };
        info.set_finished_for_test(finished);
        info
    }
    #[test]
    fn classify_subagent_running() {
        let info = make_subagent("a", false, None);
        assert_eq!(classify_subagent(&info), RowState::Working);
    }
    #[test]
    fn full_tree_excludes_workflow_owned_subagent_rows() {
        let mut agents = IndexMap::new();
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        let mut workflow_child = make_subagent("workflow-child", false, None);
        workflow_child.attempt.workflow_run_id = Some(Arc::from("wf_1"));
        agent
            .subagent_sessions
            .insert("workflow-child".into(), workflow_child);
        agents.insert(AgentId(0), agent);
        let rows = build_rows(
            &agents,
            &Default::default(),
            &[],
            super::super::state::Grouping::State,
            &Filter::default(),
            None,
        );
        assert!(
            rows.iter()
                .all(|row| !matches!(row.id, DashboardRowId::Subagent { .. }))
        );
    }
    #[test]
    fn classify_subagent_completed() {
        let info = make_subagent("a", true, Some("completed"));
        assert_eq!(classify_subagent(&info), RowState::Completed);
    }
    #[test]
    fn classify_subagent_failed() {
        let info = make_subagent("a", true, Some("failed"));
        assert_eq!(classify_subagent(&info), RowState::Failed);
    }
    #[test]
    fn classify_subagent_cancelled() {
        let info = make_subagent("a", true, Some("cancelled"));
        assert_eq!(classify_subagent(&info), RowState::Failed);
    }
    #[test]
    fn subagent_activity_prefers_live_label_over_tool_reconstruction() {
        let mut info = make_subagent("a", false, None);
        info.attempt.tools_used = vec![Arc::from("bash")];
        info.attempt.activity_label = Some("Running: cargo build".into());
        assert_eq!(
            subagent_activity(&info, RowState::Working).as_deref(),
            Some("Running: cargo build")
        );
        info.attempt.activity_label = None;
        assert_eq!(
            subagent_activity(&info, RowState::Working).as_deref(),
            Some("Running: bash")
        );
    }
    /// Subagent rows never reach `NeedsInput` in v1.
    #[test]
    fn subagent_classifier_never_emits_needs_input() {
        for finished in [false, true] {
            for status in [None, Some("completed"), Some("failed"), Some("cancelled")] {
                let info = make_subagent("a", finished, status);
                let state = classify_subagent(&info);
                assert_ne!(
                    state,
                    RowState::NeedsInput,
                    "finished={finished} status={status:?}"
                );
            }
        }
    }
    #[test]
    fn cluster_keeps_subagents_with_parent() {
        let rows = vec![
            make_row("a", 0, RowState::Working),
            make_row("a:1", 1, RowState::Working),
            make_row("a:2", 1, RowState::Idle),
            make_row("b", 0, RowState::Idle),
            make_row("b:1", 1, RowState::Working),
        ];
        let clusters = build_clusters(&rows);
        assert_eq!(clusters, vec![(0, 3), (3, 5)]);
    }
    fn make_row(label: &str, indent: u8, state: RowState) -> DashboardRow {
        DashboardRow {
            id: if indent == 0 {
                DashboardRowId::TopLevel(AgentId(label.len()))
            } else {
                DashboardRowId::Subagent {
                    parent: AgentId(0),
                    child_session_id: label.to_string(),
                }
            },
            session_id: None,
            label: label.to_string(),
            subtitle: None,
            state,
            activity: None,
            secondary_line: None,
            cwd_display: "~/x".to_string(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }
    }
    fn make_row_with_id(id: DashboardRowId, indent: u8, state: RowState) -> DashboardRow {
        DashboardRow {
            id,
            session_id: None,
            label: "row".to_string(),
            subtitle: None,
            state,
            activity: None,
            secondary_line: None,
            cwd_display: "~/x".to_string(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }
    }
    /// Sort: pinned floats above non-pinned regardless of state.
    #[test]
    fn sort_pinned_first() {
        let mut rows = vec![
            DashboardRow {
                pinned: false,
                state: RowState::Working,
                ..make_row_with_id(DashboardRowId::TopLevel(AgentId(2)), 0, RowState::Working)
            },
            DashboardRow {
                pinned: true,
                state: RowState::Idle,
                ..make_row_with_id(DashboardRowId::TopLevel(AgentId(1)), 0, RowState::Idle)
            },
        ];
        sort_rows(&mut rows, super::super::state::Grouping::State, &[]);
        assert!(nth(&rows, 0).pinned);
        assert!(!nth(&rows, 1).pinned);
    }
    #[test]
    fn sort_state_priority_within_group() {
        let mut rows = vec![
            make_row_with_id(DashboardRowId::TopLevel(AgentId(1)), 0, RowState::Idle),
            make_row_with_id(
                DashboardRowId::TopLevel(AgentId(2)),
                0,
                RowState::NeedsInput,
            ),
            make_row_with_id(DashboardRowId::TopLevel(AgentId(3)), 0, RowState::Working),
        ];
        sort_rows(&mut rows, super::super::state::Grouping::State, &[]);
        assert_eq!(nth(&rows, 0).state, RowState::NeedsInput);
        assert_eq!(nth(&rows, 1).state, RowState::Working);
        assert_eq!(nth(&rows, 2).state, RowState::Idle);
    }
    /// Renamed from `sort_deterministic_with_equal_keys`. The original name implied a tiebreak
    /// guarantee that `sort_cluster_key` does NOT provide; documents the actual behavioural contract:
    /// idempotent on identical inputs.
    #[test]
    fn sort_is_idempotent_for_identical_inputs() {
        let now = SystemTime::now();
        let mut rows1 = vec![
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(DashboardRowId::TopLevel(AgentId(1)), 0, RowState::Idle)
            },
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(DashboardRowId::TopLevel(AgentId(2)), 0, RowState::Idle)
            },
        ];
        let mut rows2 = rows1.clone();
        sort_rows(&mut rows1, super::super::state::Grouping::State, &[]);
        sort_rows(&mut rows2, super::super::state::Grouping::State, &[]);
        assert_eq!(
            rows1.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            rows2.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
        );
    }
    /// When every other field ties, the sort tiebreaks by id, producing a fully deterministic order independent of the input row order.
    /// AgentId(1) sorts before AgentId(2) regardless of which appears first in the input vector.
    #[test]
    fn sort_rows_tiebreaks_by_id_when_keys_equal() {
        let now = SystemTime::now();
        let id1 = DashboardRowId::TopLevel(AgentId(1));
        let id2 = DashboardRowId::TopLevel(AgentId(2));
        let mut forward = vec![
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(id1.clone(), 0, RowState::Idle)
            },
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(id2.clone(), 0, RowState::Idle)
            },
        ];
        let mut reverse = vec![
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(id2.clone(), 0, RowState::Idle)
            },
            DashboardRow {
                last_change_at: now,
                ..make_row_with_id(id1.clone(), 0, RowState::Idle)
            },
        ];
        sort_rows(&mut forward, super::super::state::Grouping::State, &[]);
        sort_rows(&mut reverse, super::super::state::Grouping::State, &[]);
        assert_eq!(nth(&forward, 0).id, id1, "forward: id1 must sort first");
        assert_eq!(nth(&forward, 1).id, id2, "forward: id2 must sort second");
        assert_eq!(nth(&reverse, 0).id, id1, "reverse: id1 must sort first");
        assert_eq!(nth(&reverse, 1).id, id2, "reverse: id2 must sort second");
    }
    #[test]
    fn filter_substring_label_match() {
        let mut rows = vec![
            DashboardRow {
                label: "fix login bug".to_string(),
                ..make_row("a", 0, RowState::Working)
            },
            DashboardRow {
                label: "investigate cache".to_string(),
                ..make_row("b", 0, RowState::Working)
            },
        ];
        apply_filter(&mut rows, &Filter::Substring("login".into()), None);
        assert_eq!(rows.len(), 1);
    }
    #[test]
    fn filter_agent_matches_label_and_parent() {
        let mut rows = vec![
            DashboardRow {
                label: "implementer".to_string(),
                parent_label: None,
                ..make_row("a", 0, RowState::Working)
            },
            DashboardRow {
                label: "explore".to_string(),
                parent_label: Some("implementer".to_string()),
                ..make_row("a:1", 1, RowState::Working)
            },
            DashboardRow {
                label: "other".to_string(),
                parent_label: None,
                ..make_row("b", 0, RowState::Working)
            },
        ];
        apply_filter(&mut rows, &Filter::Agent("implementer".into()), None);
        assert_eq!(rows.len(), 2);
    }
    #[test]
    fn filter_state_matches_exact() {
        let mut rows = vec![
            DashboardRow {
                state: RowState::NeedsInput,
                ..make_row("a", 0, RowState::NeedsInput)
            },
            DashboardRow {
                state: RowState::Idle,
                ..make_row("b", 0, RowState::Idle)
            },
        ];
        apply_filter(&mut rows, &Filter::State(RowState::Idle), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(nth(&rows, 0).state, RowState::Idle);
    }
    /// Sort: explicit reorderings (Shift+↑/↓) float a row to its declared position WITHIN its state group.
    /// Two Working rows: the older one (id2) is reordered above the more-recent one (id1).
    #[test]
    fn sort_reorder_floats_within_state_group() {
        let id1 = DashboardRowId::TopLevel(AgentId(1));
        let id2 = DashboardRowId::TopLevel(AgentId(2));
        let newer = SystemTime::now();
        let older = newer - Duration::from_secs(60);
        let mut rows = vec![
            DashboardRow {
                last_change_at: newer, // recency would put id1 first
                ..make_row_with_id(id1.clone(), 0, RowState::Working)
            },
            DashboardRow {
                last_change_at: older,
                ..make_row_with_id(id2.clone(), 0, RowState::Working)
            },
        ];
        sort_rows(
            &mut rows,
            super::super::state::Grouping::State,
            std::slice::from_ref(&id2),
        );
        assert_eq!(nth(&rows, 0).id, id2, "reorder must float id2 to the top");
        assert_eq!(nth(&rows, 1).id, id1);
    }
    /// Regression: reordering must NOT split a state group. The renderer would otherwise emit `Idle →
    /// Working → Idle` headers. The reorder only floats the Idle row WITHIN the Idle group.
    #[test]
    fn reorder_does_not_split_state_groups() {
        let w1 = DashboardRowId::TopLevel(AgentId(1));
        let i1 = DashboardRowId::TopLevel(AgentId(2));
        let w2 = DashboardRowId::TopLevel(AgentId(3));
        let i2 = DashboardRowId::TopLevel(AgentId(4));
        let mut rows = vec![
            make_row_with_id(w1, 0, RowState::Working),
            make_row_with_id(i1.clone(), 0, RowState::Idle),
            make_row_with_id(w2, 0, RowState::Working),
            make_row_with_id(i2.clone(), 0, RowState::Idle),
        ];
        sort_rows(
            &mut rows,
            super::super::state::Grouping::State,
            &[i1.clone(), i2.clone()],
        );
        let states: Vec<RowState> = rows.iter().map(|r| r.state).collect();
        assert_eq!(
            states,
            vec![
                RowState::Working,
                RowState::Working,
                RowState::Idle,
                RowState::Idle,
            ],
            "reorder must not interleave states, got {states:?}",
        );
        let idle_order: Vec<DashboardRowId> = rows
            .iter()
            .filter(|r| r.state == RowState::Idle)
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(idle_order, vec![i1, i2], "reorder must order within Idle");
    }
    #[test]
    fn pinned_manual_order_survives_activity_changes() {
        for grouping in [
            super::super::state::Grouping::State,
            super::super::state::Grouping::Directory,
        ] {
            let first = DashboardRowId::TopLevel(AgentId(2));
            let second = DashboardRowId::TopLevel(AgentId(1));
            let reorder = vec![first.clone(), second.clone()];
            let mut rows = vec![
                DashboardRow {
                    pinned: true,
                    cwd_display: "/z".to_owned(),
                    last_change_at: UNIX_EPOCH,
                    ..make_row_with_id(first, 0, RowState::Idle)
                },
                DashboardRow {
                    pinned: true,
                    cwd_display: "/a".to_owned(),
                    last_change_at: UNIX_EPOCH,
                    ..make_row_with_id(second, 0, RowState::Idle)
                },
            ];
            for state in [
                RowState::Idle,
                RowState::Working,
                RowState::NeedsInput,
                RowState::Idle,
            ] {
                let row = rows.last_mut().expect("second pinned row");
                row.state = state;
                row.last_change_at += Duration::from_secs(60);
                sort_rows(&mut rows, grouping, &reorder);
                assert_eq!(
                    reorder,
                    rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>()
                );
            }
        }
    }
    #[test]
    fn unranked_pins_use_session_id_then_row_id() {
        let cases = [
            (None, None, [1, 2]),
            (None, Some("a"), [1, 2]),
            (Some("a"), None, [2, 1]),
            (Some("b"), Some("a"), [2, 1]),
        ];
        for grouping in [
            super::super::state::Grouping::State,
            super::super::state::Grouping::Directory,
        ] {
            for (first_session, second_session, expected) in cases {
                let mut rows = [(2, second_session), (1, first_session)]
                    .into_iter()
                    .map(|(id, session_id)| DashboardRow {
                        pinned: true,
                        session_id: session_id.map(str::to_owned),
                        last_change_at: UNIX_EPOCH,
                        ..make_row_with_id(DashboardRowId::TopLevel(AgentId(id)), 0, RowState::Idle)
                    })
                    .collect::<Vec<_>>();
                sort_rows(&mut rows, grouping, &[]);
                assert_eq!(
                    expected
                        .map(|id| DashboardRowId::TopLevel(AgentId(id)))
                        .as_slice(),
                    rows.into_iter()
                        .map(|row| row.id)
                        .collect::<Vec<_>>()
                        .as_slice(),
                    "{grouping:?}: {first_session:?}, {second_session:?}",
                );
            }
        }
    }
    #[test]
    fn pinned_session_order_survives_roster_activity_changes() {
        let mut local = crate::app::agent_view::test_fixtures::make_agent();
        local.session.session_id = Some(acp::SessionId::new("z-local"));
        local.display_name = Some("Local".to_owned());
        let agents = IndexMap::from([(AgentId(1), local)]);
        let roster_id = DashboardRowId::Roster {
            session_id: "a-roster".to_owned(),
        };
        let local_id = DashboardRowId::TopLevel(AgentId(1));
        let pinned = std::collections::BTreeSet::from([local_id.clone(), roster_id.clone()]);
        let mut roster = roster_entry("a-roster", 1_000);
        for grouping in [
            super::super::state::Grouping::State,
            super::super::state::Grouping::Directory,
        ] {
            for activity in [RosterActivity::Working, RosterActivity::Dormant] {
                roster.activity = activity;
                roster.last_change_unix_ms += 1_000;
                let rows = build_rows_with_roster(
                    &agents,
                    &pinned,
                    &[],
                    grouping,
                    &Filter::None,
                    None,
                    std::slice::from_ref(&roster),
                );
                assert_eq!(
                    vec![roster_id.clone(), local_id.clone()],
                    rows.into_iter().map(|row| row.id).collect::<Vec<_>>(),
                );
            }
        }
    }
    #[test]
    fn pinned_workspace_order_survives_session_loading() {
        for grouping in [
            distill_dashboard_store::Grouping::State,
            distill_dashboard_store::Grouping::Directory,
        ] {
            for manual_order in [false, true] {
                let mut a = workspace_member("a", "A", None);
                let mut z = workspace_member("z", "Z", None);
                a.pin_rank = Some(1024);
                z.pin_rank = Some(1024);
                a.cwd = Some("/z".to_owned());
                z.cwd = Some("/a".to_owned());
                if manual_order {
                    a.order_rank = Some(2048);
                    z.order_rank = Some(1024);
                }
                let mut snapshot = workspace_snapshot(vec![a, z]);
                snapshot.grouping = grouping.clone();
                let expected = if manual_order {
                    vec!["z", "a"]
                } else {
                    vec!["a", "z"]
                };
                let mut agents = IndexMap::new();
                for loaded in [false, true, false] {
                    if loaded {
                        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
                        agent.session.session_id = Some(acp::SessionId::new("z"));
                        agent.session.cwd = PathBuf::from("/a");
                        agent.session.enqueue_prompt("work".to_owned());
                        agents.insert(AgentId(1), agent);
                    } else {
                        agents.clear();
                    }
                    let rows = workspace_rows(&agents, &snapshot);
                    assert_eq!(
                        expected,
                        rows.iter()
                            .map(|row| row.session_id.as_deref().expect("saved session"))
                            .collect::<Vec<_>>(),
                    );
                    assert_eq!(
                        loaded,
                        rows.iter()
                            .any(|row| matches!(row.id, DashboardRowId::TopLevel(_)))
                    );
                }
            }
        }
    }
    /// Filter: agent prefix with empty needle keeps everything (treated as `Filter::None` upstream by `Filter::from_value`).
    #[test]
    fn filter_agent_with_empty_string_does_not_hide_rows() {
        let mut rows = vec![
            make_row("a", 0, RowState::Working),
            make_row("b", 0, RowState::Idle),
        ];
        apply_filter(&mut rows, &Filter::Agent(String::new()), None);
        assert_eq!(rows.len(), 2);
    }
    /// More-placeholder rows survive filtering.
    #[test]
    fn filter_keeps_more_placeholders() {
        let mut rows = vec![
            DashboardRow {
                label: "real row".to_string(),
                ..make_row("a", 1, RowState::Idle)
            },
            DashboardRow {
                label: "… 3 more".to_string(),
                is_more_placeholder: true,
                ..make_row("b", 1, RowState::Idle)
            },
        ];
        apply_filter(
            &mut rows,
            &Filter::Substring("nothing matches".into()),
            None,
        );
        assert!(rows.iter().any(|r| r.is_more_placeholder));
    }
    fn roster_entry(session_id: &str, last_change_unix_ms: i64) -> RosterEntry {
        use crate::app::roster::RosterOrigin;
        RosterEntry {
            session_id: session_id.to_string(),
            title: Some(format!("title {session_id}")),
            cwd: "/repo".to_string(),
            is_worktree: false,
            session_kind: None,
            model_id: None,
            yolo: false,
            activity: RosterActivity::Dormant,
            last_turn_summary: None,
            resident: false,
            last_change_unix_ms,
            origin: RosterOrigin::default(),
        }
    }
    fn now_unix_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }
    /// The age rendered for a roster row (`render` uses `last_change_at.elapsed()`).
    fn roster_row_age(entry: RosterEntry) -> Duration {
        let mut rows = Vec::new();
        append_roster_rows(
            &mut rows,
            &[entry],
            &IndexMap::new(),
            &std::collections::BTreeSet::new(),
            None,
        );
        assert_eq!(rows.len(), 1);
        nth(&rows, 0).last_change_at.elapsed().unwrap_or_default()
    }
    /// A roster entry touched a while ago must NOT render as "just now".
    /// Its `last_change_at` is derived from `last_change_unix_ms`, so the age reflects the real wall-clock age.
    /// Regression guard for the bug where every roster row stamped `Instant::now()`.
    #[test]
    fn roster_row_age_reflects_last_change_unix_ms() {
        let two_hours_ago = now_unix_ms() - 2 * 3_600_000;
        let elapsed = roster_row_age(roster_entry("sess-old", two_hours_ago));
        assert!(
            elapsed.as_secs() >= 3600,
            "roster row age should reflect the real timestamp, got {elapsed:?}"
        );
        assert_eq!(crate::util::format_time_ago(elapsed), "2h");
    }
    /// Roster rows sort by their real `last_change_unix_ms`: a session touched long ago floats below a recently-touched one.
    #[test]
    fn roster_rows_sort_by_real_timestamp() {
        let now_ms = now_unix_ms();
        let mut rows = Vec::new();
        let agents = IndexMap::new();
        let pinned = std::collections::BTreeSet::new();
        append_roster_rows(
            &mut rows,
            &[
                roster_entry("sess-old", now_ms - 86_400_000),
                roster_entry("sess-new", now_ms - 5_000),
            ],
            &agents,
            &pinned,
            None,
        );
        sort_rows(&mut rows, super::super::state::Grouping::State, &[]);
        assert_eq!(
            nth(&rows, 0).id,
            DashboardRowId::Roster {
                session_id: "sess-new".to_string()
            }
        );
        assert_eq!(
            nth(&rows, 1).id,
            DashboardRowId::Roster {
                session_id: "sess-old".to_string()
            }
        );
    }
    /// Build a roster entry with an explicit title and activity for the empty-session filtering tests.
    fn roster_entry_with(
        session_id: &str,
        title: Option<&str>,
        activity: RosterActivity,
    ) -> RosterEntry {
        RosterEntry {
            title: title.map(String::from),
            activity,
            ..roster_entry(session_id, now_unix_ms())
        }
    }
    fn collect_roster(
        entries: &[RosterEntry],
        pinned: &std::collections::BTreeSet<DashboardRowId>,
    ) -> Vec<DashboardRow> {
        let mut rows = Vec::new();
        append_roster_rows(&mut rows, entries, &IndexMap::new(), pinned, None);
        rows
    }
    /// An untitled, inactive roster session is the "New session" noise the dashboard hides; every pager launch leaves one behind.
    #[test]
    fn append_roster_rows_hides_untitled_idle_entry() {
        let empty = std::collections::BTreeSet::new();
        for activity in [
            RosterActivity::Idle,
            RosterActivity::Dormant,
            RosterActivity::Completed,
            RosterActivity::Dead,
        ] {
            let rows = collect_roster(&[roster_entry_with("e", None, activity)], &empty);
            assert!(
                rows.is_empty(),
                "untitled {activity:?} entry must be hidden"
            );
        }
    }
    /// An untitled session that is actively working / awaiting input stays visible: its title hasn't been generated yet but it is doing real work.
    #[test]
    fn append_roster_rows_keeps_untitled_active_entry() {
        let empty = std::collections::BTreeSet::new();
        for activity in [RosterActivity::Working, RosterActivity::NeedsInput] {
            let rows = collect_roster(&[roster_entry_with("a", None, activity)], &empty);
            assert_eq!(rows.len(), 1, "untitled {activity:?} entry must be kept");
        }
    }
    /// Titled (real) sessions always render, even when idle/dormant.
    #[test]
    fn append_roster_rows_keeps_titled_entry() {
        let empty = std::collections::BTreeSet::new();
        let rows = collect_roster(
            &[roster_entry_with(
                "t",
                Some("Fix the bug"),
                RosterActivity::Dormant,
            )],
            &empty,
        );
        assert_eq!(rows.len(), 1, "titled entries are kept");
    }
    /// A blank/whitespace title is treated as no title.
    #[test]
    fn append_roster_rows_hides_blank_titled_idle_entry() {
        let empty = std::collections::BTreeSet::new();
        let rows = collect_roster(
            &[roster_entry_with("b", Some("   "), RosterActivity::Idle)],
            &empty,
        );
        assert!(rows.is_empty(), "blank-title idle entry must be hidden");
    }
    /// A user-pinned row is always shown, even untitled and idle.
    #[test]
    fn append_roster_rows_keeps_pinned_untitled_entry() {
        let mut pinned = std::collections::BTreeSet::new();
        pinned.insert(DashboardRowId::Roster {
            session_id: "p".to_string(),
        });
        let rows = collect_roster(
            &[roster_entry_with("p", None, RosterActivity::Idle)],
            &pinned,
        );
        assert_eq!(rows.len(), 1, "pinned untitled entries are kept");
    }
    /// A roster session with a known model shows it on the otherwise blank second line.
    /// The wire carries no last-message preview, so the model is the only per-session detail left to show.
    #[test]
    fn append_roster_rows_uses_model_id_as_secondary_line() {
        let empty = std::collections::BTreeSet::new();
        let entry = RosterEntry {
            model_id: Some("grok-4.5".to_string()),
            ..roster_entry_with("m", Some("Fix the bug"), RosterActivity::Dormant)
        };
        let rows = collect_roster(&[entry], &empty);
        assert_eq!(rows.len(), 1);
        assert_eq!(nth(&rows, 0).secondary_line.as_deref(), Some("grok-4.5"));
    }
    /// The last-turn summary wins the secondary line over the model id.
    #[test]
    fn append_roster_rows_prefers_last_turn_summary() {
        let empty = std::collections::BTreeSet::new();
        let entry = RosterEntry {
            model_id: Some("grok-4.5".to_string()),
            last_turn_summary: Some("Fixed the roster merge".to_string()),
            ..roster_entry_with("m", Some("Fix the bug"), RosterActivity::Dormant)
        };
        let rows = collect_roster(&[entry], &empty);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            nth(&rows, 0).secondary_line.as_deref(),
            Some("Fixed the roster merge")
        );
    }
    /// Without a model id there's nothing to show, so the second line stays empty rather than rendering a placeholder.
    #[test]
    fn append_roster_rows_secondary_line_none_without_model() {
        let empty = std::collections::BTreeSet::new();
        let rows = collect_roster(
            &[roster_entry_with(
                "m",
                Some("Fix the bug"),
                RosterActivity::Dormant,
            )],
            &empty,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(nth(&rows, 0).secondary_line, None);
    }
    /// Build a minimal idle local agent with an empty scrollback (no replayed messages) and an optional current model.
    /// It mirrors a session listed on the dashboard that the user hasn't opened yet.
    fn make_idle_agent_with_model(model: Option<&str>) -> AgentView {
        use crate::acp::model_state::ModelState;
        use crate::app::agent::{AgentSession, AgentState};
        use crate::scrollback::state::ScrollbackState;
        use agent_client_protocol as acp;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut models = ModelState::default();
        if let Some(m) = model {
            models.set_current(acp::ModelId::new(std::sync::Arc::from(m)), None);
        }
        let session = AgentSession {
            id: AgentId(0),
            acp_tx: tx,
            session_id: Some(acp::SessionId::new("test-session")),
            models,
            state: AgentState::Idle,
            tracker: crate::acp::tracker::AcpUpdateTracker::new(),
            cwd: PathBuf::from("/tmp"),
            is_worktree: false,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: false,
            auto_mode: false,
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: Vec::new(),
            available_commands_generation: 0,
            available_tools: None,
            model_switch_pending: false,
            hook_block_hold: false,
            blocked_prompt: None,
            user_model_preference: None,
            deferred_model_switch: None,
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            compact_held_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        };
        AgentView::new(session, ScrollbackState::new())
    }
    /// An idle local agent with no last message has a BLANK second line.
    /// The model now shows in the peek panel's bottom-border badge for the selected row instead, keeping the list uncluttered.
    #[test]
    fn idle_local_agent_without_message_has_blank_secondary() {
        let agent = make_idle_agent_with_model(Some("grok-4.5"));
        assert_eq!(classify_top_level(&agent), RowState::Idle);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(
            row.secondary_line, None,
            "the model must not appear on the list row anymore",
        );
    }
    /// When neither a message nor a model is known the second line is left empty (no placeholder).
    #[test]
    fn idle_local_agent_without_message_or_model_has_no_secondary() {
        let agent = make_idle_agent_with_model(None);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(row.secondary_line, None);
    }
    /// A worktree agent's subtitle shows the branch, the worktree's human label, and a `worktree` marker.
    #[test]
    fn subtitle_worktree_shows_label_branch_and_marker() {
        let mut agent = make_idle_agent_with_model(None);
        agent.session.cwd = PathBuf::from("/home/me/.grok/worktrees/x/location-picker");
        agent.is_worktree = true;
        agent.worktree_label = Some("location-picker".to_string());
        agent.current_branch = Some("kevin/feature".to_string());
        assert_eq!(
            top_level_subtitle(&agent).as_deref(),
            Some("kevin/feature location-picker worktree"),
        );
    }
    /// A worktree without a DB label falls back to the cwd folder name.
    #[test]
    fn subtitle_worktree_without_label_uses_folder_name() {
        let mut agent = make_idle_agent_with_model(None);
        agent.session.cwd = PathBuf::from("/home/me/wt/my-wt-dir");
        agent.is_worktree = true;
        agent.worktree_label = None;
        agent.current_branch = Some("main".to_string());
        assert_eq!(
            top_level_subtitle(&agent).as_deref(),
            Some("main my-wt-dir worktree"),
        );
    }
    /// A non-worktree agent shows the branch and the cwd folder name (the actual working subdir, not the repo root), with no worktree marker.
    #[test]
    fn subtitle_non_worktree_shows_branch_and_folder() {
        let mut agent = make_idle_agent_with_model(None);
        agent.session.cwd = PathBuf::from("/home/me/xai/crates/foo");
        agent.is_worktree = false;
        agent.current_branch = Some("main".to_string());
        assert_eq!(top_level_subtitle(&agent).as_deref(), Some("main foo"));
    }
    /// With no branch known, the subtitle is just the folder name.
    #[test]
    fn subtitle_no_branch_shows_only_folder() {
        let mut agent = make_idle_agent_with_model(None);
        agent.session.cwd = PathBuf::from("/home/me/projects/bar");
        agent.is_worktree = false;
        agent.current_branch = None;
        assert_eq!(top_level_subtitle(&agent).as_deref(), Some("bar"));
    }
    /// Task counts reflect running background commands only.
    /// `bg_tasks` retains finished (`Done` / `Failed`) tasks for the tasks-pane history, and those must not pin a stale badge on the row forever.
    #[test]
    fn task_chip_only_for_running_tasks() {
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        let make_task = |status: BgTaskStatus| BgTaskState {
            task_id: "t1".into(),
            tool_call_id: String::new(),
            command: "sleep 99".into(),
            description: None,
            cwd: String::new(),
            output_file: String::new(),
            status,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor: false,
            restored_from_replay: false,
        };
        for (status, expect_badge) in [
            (BgTaskStatus::Running, true),
            (BgTaskStatus::Done, false),
            (BgTaskStatus::Failed, false),
        ] {
            let mut agent = make_idle_agent_with_model(None);
            agent
                .session
                .bg_tasks
                .insert("t1".into(), make_task(status));
            let row = top_level_row(AgentId(0), &agent, false, None);
            assert_eq!(
                row.badges.contains(&RowBadge::Tasks(1)),
                expect_badge,
                "status={status:?} badge expectation",
            );
        }
    }
    /// A turn-idle agent with a RUNNING background task is `Working`, not `Idle`.
    /// Finished tasks (Done / Failed) don't keep it Working; they linger in `bg_tasks` only for the tasks-pane history.
    #[test]
    fn running_bg_task_classifies_as_working() {
        use crate::app::agent::BgTaskStatus;
        for (status, expected) in [
            (BgTaskStatus::Running, RowState::Working),
            (BgTaskStatus::Done, RowState::Idle),
            (BgTaskStatus::Failed, RowState::Idle),
        ] {
            let mut agent = make_idle_agent_with_model(None);
            let mut task = running_bg_task("t1", false);
            task.status = status;
            agent.session.bg_tasks.insert("t1".into(), task);
            assert_eq!(
                classify_top_level(&agent),
                expected,
                "status={status:?} must classify as {expected:?}",
            );
        }
    }
    /// A running monitor keeps the agent Working without implying a live parent turn.
    #[test]
    fn running_monitor_classifies_as_working_with_chip() {
        let mut agent = make_idle_agent_with_model(None);
        agent
            .session
            .bg_tasks
            .insert("m1".into(), running_bg_task("m1", true));
        assert_eq!(classify_top_level(&agent), RowState::Working);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(vec![RowBadge::Watchers(1)], row.badges);
        assert_eq!(None, row.activity);
    }
    /// A scheduled loop keeps its watcher chip between fires.
    #[test]
    fn scheduled_loop_classifies_as_working_with_chip() {
        let mut agent = make_idle_agent_with_model(None);
        agent
            .session
            .scheduled_tasks
            .insert("l1".into(), scheduled_loop("l1"));
        assert_eq!(classify_top_level(&agent), RowState::Working);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(vec![RowBadge::Watchers(1)], row.badges);
        assert_eq!(None, row.activity);
    }
    /// Monitors and loops share a watcher count; commands remain separate.
    #[test]
    fn live_work_badges_group_watchers_apart_from_tasks() {
        let mut agent = make_idle_agent_with_model(None);
        agent
            .session
            .bg_tasks
            .insert("m1".into(), running_bg_task("m1", true));
        agent
            .session
            .bg_tasks
            .insert("b1".into(), running_bg_task("b1", false));
        agent
            .session
            .bg_tasks
            .insert("b2".into(), running_bg_task("b2", false));
        agent
            .session
            .scheduled_tasks
            .insert("l1".into(), scheduled_loop("l1"));
        assert_eq!(classify_top_level(&agent), RowState::Working);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(vec![RowBadge::Tasks(2), RowBadge::Watchers(2)], row.badges);
    }
    /// Replay loading remains live activity while background work is present.
    #[test]
    fn loading_activity_remains_live_with_background_work() {
        let mut agent = make_idle_agent_with_model(None);
        agent.session.loading_replay = true;
        agent
            .session
            .bg_tasks
            .insert("m1".into(), running_bg_task("m1", true));
        assert_eq!(classify_top_level(&agent), RowState::Working);
        let row = top_level_row(AgentId(0), &agent, false, None);
        assert_eq!(
            row.activity.as_deref(),
            Some("Loading…"),
            "replay loading must keep its activity",
        );
    }
    /// Roster-only idle / dormant sessions classify as `Inactive`, the dedicated section for sessions
    /// not loaded in this pager. Local idle agents stay `Idle`; `classify_top_level` never returns
    /// `Inactive`.
    #[test]
    fn roster_idle_and_dormant_classify_as_inactive() {
        let empty = std::collections::BTreeSet::new();
        for (activity, expected) in [
            (RosterActivity::Idle, RowState::Inactive),
            (RosterActivity::Dormant, RowState::Inactive),
            (RosterActivity::Working, RowState::Working),
            (RosterActivity::NeedsInput, RowState::NeedsInput),
            (RosterActivity::Completed, RowState::Completed),
            (RosterActivity::Dead, RowState::Failed),
        ] {
            let rows = collect_roster(&[roster_entry_with("m", Some("Task"), activity)], &empty);
            assert_eq!(rows.len(), 1, "activity={activity:?}");
            assert_eq!(nth(&rows, 0).state, expected, "activity={activity:?}");
        }
    }
    /// In state grouping, `Inactive` sorts below `Idle`: not loaded here, so less immediately actionable.
    /// It sorts above `Done` / `Failed`: still live, resumable sessions.
    #[test]
    fn sort_rows_places_inactive_between_idle_and_done() {
        let mut rows = vec![
            make_row("done", 0, RowState::Completed),
            make_row("inactive", 0, RowState::Inactive),
            make_row("idle", 0, RowState::Idle),
            make_row("failed", 0, RowState::Failed),
        ];
        sort_rows(&mut rows, super::super::state::Grouping::State, &[]);
        let order: Vec<RowState> = rows.iter().map(|r| r.state).collect();
        assert_eq!(
            order,
            vec![
                RowState::Idle,
                RowState::Inactive,
                RowState::Completed,
                RowState::Failed,
            ],
        );
    }
    #[test]
    fn workspace_manual_order_is_global_and_survives_filtering() {
        let mut a = workspace_member("a", "Alpha", None);
        a.order_rank = Some(2 * distill_dashboard_store::RANK_GAP);
        let b = workspace_member("b", "Beta", None);
        let mut c = workspace_member("c", "Charlie", None);
        c.order_rank = Some(distill_dashboard_store::RANK_GAP);
        let snapshot = workspace_snapshot(vec![a, b, c]);
        let rows = workspace_rows(&IndexMap::new(), &snapshot);
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Charlie", "Alpha", "Beta"]
        );
        let workspace = crate::app::workspace_layout::WorkspaceView::from_snapshot(&snapshot);
        let rows = build_rows_with_workspace(
            &IndexMap::new(),
            WorkspaceRowInputs {
                workspace: Some(&workspace),
                provisional: &[],
            },
            &Filter::Substring("l".into()),
            None,
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Charlie", "Alpha"]
        );
    }
    #[test]
    fn workspace_rows_use_store_layout_for_loaded_and_unloaded_pins() {
        let mut stored = workspace_member("saved", "", None);
        stored.pin_rank = Some(7);
        stored.order_rank = Some(9);
        let snapshot = workspace_snapshot(vec![stored]);
        let rows = workspace_rows(&IndexMap::new(), &snapshot);
        assert!(nth(&rows, 0).pinned);
        assert!(nth(&rows, 0).badges.contains(&RowBadge::Pinned));
        let agent_id = AgentId(9);
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        agent.session.session_id = Some(acp::SessionId::new("saved"));
        agent.session.created_via_new = true;
        let agents = IndexMap::from([(agent_id, agent)]);
        let rows = workspace_rows(&agents, &snapshot);
        assert_eq!(rows.len(), 1);
        assert!(nth(&rows, 0).pinned);
    }
}
