// Modified for Distill by Samuel Fajreldines, 2026.
//! Minimal-mode commit pipeline: which finalized blocks get printed into the terminal's native scrollback, and in what display mode.
//!
//! The module stays terminal-agnostic and unit-testable: the caller injects the actual `insert_before` call as a closure.
//! The "committed frontier" is the leading contiguous run of finalized, non-pending entries.
//! Everything past it stays in the live region until it finalizes.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use distill_pager::app::PagerTerminal;
use distill_pager::app::app_view::{ActiveView, AppView};
use distill_pager::appearance::AppearanceConfig;
use distill_pager::minimal_api;
use distill_pager::render::Renderable;
use distill_pager::scrollback::block::RenderBlock;
use distill_pager::scrollback::blocks::ToolCallBlock;
use distill_pager::scrollback::entry::{EntryId, ScrollbackEntry};
use distill_pager::scrollback::state::ScrollbackState;
use distill_pager::scrollback::types::{BlockBackground, DisplayMode};
use distill_pager::scrollback::wrappers::EntryRenderer;
use distill_pager::theme::Theme;

/// Whatever the value, both sides of the commit frontier must apply it identically ([`super::live::draw_tail`],
/// [`super::live::tail_height`]). Otherwise a block's height would change as it moves into native scrollback and
/// the prompt would shift on every commit.
pub(crate) const MINIMAL_BLOCK_GAP: u16 = 0;

/// Mid-turn, commit only finished entries not awaiting input; a pending tool stays in the live tail.
/// Agent messages stay `is_running` until turn end: a later non-thinking entry closes them, interleaved thinking does not (or the first words freeze). `BgTask` commits immediately — content never changes and it can outlive the turn.
/// Once idle, a stale `is_running` must not wedge the frontier. A running entry pushed while idle commits immediately, so placeholder fills must check `is_committed` and append a fresh block.
pub fn is_committable(state: &ScrollbackState, i: usize, turn_running: bool) -> bool {
    let Some(entry) = state.get(i) else {
        return false;
    };
    // A block awaiting user input (permission / ask_user_question) holds the frontier in EVERY turn state, not just mid-turn
    // Its rendered form still changes when the prompt resolves, so committing it (print-once) would freeze the "waiting" form on the terminal
    // The idle case is defensive: permissions normally resolve within the turn, but a pending mark must never be committed out from under its modal
    if entry.is_pending_user_input {
        return false;
    }
    if !turn_running {
        return true;
    }
    if !entry.is_running {
        return true;
    }
    // Running, mid-turn. A BgTask lifecycle block is a finalized event whose flag is animation-only. A running tool
    // may still update its result, and a still-open agent stream (last, or only followed by interleaved thinking) must
    // stay live.
    matches!(entry.block, RenderBlock::BgTask(_))
        || (matches!(entry.block, RenderBlock::AgentMessage(_))
            && agent_message_stream_closed(state, i))
}

/// Whether a later entry proves the tracker will not append to the agent message at `i` again.
/// `handle_tool_call`, a new stream, and turn-end push a tool, another message, or a session event and drop `current_agent_msg`.
/// Interleaved thinking does not: it is inserted after the live message while chunks still append, so it must not trip print-once.
fn agent_message_stream_closed(state: &ScrollbackState, i: usize) -> bool {
    ((i + 1)..state.len()).any(|j| {
        state
            .get(j)
            .is_some_and(|e| !matches!(e.block, RenderBlock::Thinking(_)))
    })
}

/// The display mode a block should be committed in (minimal mode, print-once).
/// [`commit_active`] stamps BOTH the entry being committed and the still-uncommitted live-tail entries with this mode.
/// That keeps a block's height identical on either side of the commit frontier, so the prompt does not jerk when it crosses.
pub fn minimal_commit_display_mode(
    block: &RenderBlock,
    appearance: &AppearanceConfig,
) -> DisplayMode {
    let collapse_thinking = appearance.minimal_collapse_thinking;
    match block {
        RenderBlock::ToolCall(ToolCallBlock::Edit(_)) => DisplayMode::Expanded,
        RenderBlock::ToolCall(
            tc @ (ToolCallBlock::Search(_)
            | ToolCallBlock::Read(_)
            | ToolCallBlock::ListDir(_)
            | ToolCallBlock::MemorySearch(_)
            | ToolCallBlock::IntegrationSearch(_)),
        ) if tc.is_success() => DisplayMode::Collapsed,
        RenderBlock::ToolCall(_) => DisplayMode::Truncated,
        RenderBlock::Thinking(_) if collapse_thinking => DisplayMode::Collapsed,
        RenderBlock::Thinking(_) => DisplayMode::Expanded,
        _ => DisplayMode::Expanded,
    }
}

/// One step of the frontier walk, the single classification shared by [`commit_leading_run`] and [`scan_frontier`].
/// The commit pass, the `will_commit` resize gate, the `tail_height` viewport sizing, and the tail renderer must agree on where the frontier stops.
/// Otherwise a block's height flips between the live region and native scrollback and the prompt jumps on commit.
enum Step {
    /// Uncommitted and committable: a commit pass consumes it.
    Commit,
    /// Already committed: skip over it (the id-set is authoritative; the scan cursor is only a lower-bound hint).
    Skip,
    /// End of entries, or the first uncommitted non-committable entry; the live tail starts here.
    Stop,
}

/// Classify the entry at `i` relative to the commit frontier.
fn classify(state: &ScrollbackState, i: usize, turn_running: bool) -> Step {
    match state.get(i) {
        None => Step::Stop,
        Some(e) if minimal_api::is_committed(state, e) => Step::Skip,
        Some(_) if !is_committable(state, i, turn_running) => Step::Stop,
        Some(_) => Step::Commit,
    }
}

/// Read-only projection of what a commit pass would do, for the consumers that must agree with it without running it.
pub struct FrontierScan {
    /// Index of the first entry a commit pass would NOT consume, where the live tail starts after this frame's commit.
    /// Everything from here on stays in the pinned live region.
    pub tail_start: usize,
    /// Whether a commit pass would emit at least one block into native scrollback this frame.
    pub will_commit: bool,
}

/// Walk the frontier read-only (no cursor mutation, nothing marked committed).
/// Used by the overlay host's viewport sizing ([`super::overlay::sync_viewport`] via [`super::live::tail_height`]) and its commit gate.
/// Both run *before* [`commit_active`] in the frame and must mirror its stop condition exactly.
pub fn scan_frontier(state: &ScrollbackState, turn_running: bool) -> FrontierScan {
    let mut i = minimal_api::commit_scan_cursor(state);
    let mut will_commit = false;
    loop {
        match classify(state, i, turn_running) {
            Step::Stop => break,
            Step::Skip => i += 1,
            Step::Commit => {
                will_commit = true;
                i += 1;
            }
        }
    }
    FrontierScan {
        tail_start: i,
        will_commit,
    }
}

/// A `false` return (a failed terminal write) stops the walk with the entry uncommitted and the cursor before it,
/// so the next frame retries. Print-once can never re-emit a block marked committed but never printed.
pub fn commit_leading_run(
    state: &mut ScrollbackState,
    turn_running: bool,
    mut on_commit: impl FnMut(&mut ScrollbackState, usize) -> bool,
) -> usize {
    let mut i = minimal_api::commit_scan_cursor(state);
    let mut count = 0usize;
    loop {
        match classify(state, i, turn_running) {
            Step::Stop => break,
            Step::Skip => i += 1,
            Step::Commit => {
                if !on_commit(state, i) {
                    break; // emit failed: leave uncommitted, retry next frame
                }
                minimal_api::mark_committed(state, i);
                count += 1;
                i += 1;
            }
        }
    }
    minimal_api::set_commit_scan_cursor(state, i);
    count
}

/// Appearance used when committing blocks to native scrollback.
///
/// Timestamps are forced off: `EntryRenderer::desired_height` subtracts the timestamp column but `render` treats it as an overlay.
/// Leaving them on would make the reserved `insert_before` height disagree with the painted rows.
/// Block horizontal padding is zeroed so committed content sits flush-left with the welcome card (which paints edge-to-edge).
/// Paired with [`minimal_renderer`] reclaiming the accent column, glyphs start at column 0.
/// The live region's prompt, status, and info rows mirror that via [`super::live::live_left_inset`].
///
/// The reasoning-legibility toggles are set here rather than in `pager.toml` so the full TUI stays provably untouched.
/// `rail_under_bullet` moves the reasoning rail out of the reserved accent column and into the body rows, directly below the header's `◆`.
/// The header diamond then stays flush at column 0 like every other block's, and the gutter reads as one consistent treatment.
pub(crate) fn committed_appearance(base: &AppearanceConfig) -> AppearanceConfig {
    let mut a = base.clone();
    a.show_timestamps = false;
    a.scrollback.layout.block_pad_left = 0;
    a.scrollback.layout.block_pad_right = 0;
    a.scrollback.blocks.thinking.body_dim_italic = true;
    a.scrollback.blocks.thinking.collapsed_expand_hint = true;
    a.scrollback.blocks.thinking.rail_under_bullet = true;
    a.scrollback.blocks.prompt.bg = BlockBackground::None;
    a
}

pub(crate) const COMMITTED_TICK: u64 = 0;

/// The renderer for one minimal-mode entry, on either side of the commit frontier; `tick` is the only difference.
/// Chrome here decides a block's wrapped height, so both sides must agree or the prompt jumps on commit; one constructor keeps them agreeing.
///
/// No block reserves the accent column: reasoning's rail renders inside its body rows via `rail_under_bullet` ([`committed_appearance`]), directly below the header's bullet.
/// Every block, reasoning included, starts flush at column 0; `no_block_spends_the_accent_column` pins this.
pub(crate) fn minimal_renderer<'a>(
    entry: &'a ScrollbackEntry,
    theme: &'a Theme,
    appearance: AppearanceConfig,
    cwd: &'a std::path::Path,
    tick: u64,
) -> EntryRenderer<'a> {
    EntryRenderer::new(entry, theme)
        .with_appearance(appearance)
        .with_cwd(Some(cwd))
        .with_tick(tick)
        .with_flat_background(true)
        .with_hide_accent(true)
        // The accent resolves to `Color::Reset` under the terminal-native palette: full-brightness default fg, which would shout.
        .with_dim_accent(true)
}

/// Diffs always commit in full, so an uncapped multi-thousand-line `Edit` would allocate one huge `Buffer` and
/// writer-thread send burst. When the block is taller than `max_rows`, only the top `max_rows - 1` content rows are
/// committed.
fn insert_committed(
    terminal: &mut PagerTerminal,
    renderer: EntryRenderer<'_>,
    width: u16,
    max_rows: u16,
    footer_style: Style,
) -> std::io::Result<()> {
    let full_h = renderer.desired_height(width);
    if full_h == 0 {
        return Ok(());
    }
    let commit_h = if max_rows > 0 && full_h > max_rows {
        max_rows
    } else {
        full_h
    };
    // Off-screen paint then wrap-aware emit: a dense `insert_before` grid turns pads and soft wraps into hard breaks.
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height: commit_h,
    };
    let mut buf = ratatui::buffer::Buffer::empty(area);
    paint_committed(&mut buf, &renderer, width, full_h, footer_style);
    let wraps = commit_wrap_flags(&renderer, buf.area.height, full_h);
    let rows = super::full_view::buffer_to_semantic_rows(&buf, &wraps);
    // Propagated (not swallowed): the caller must NOT mark the entry committed when the terminal write failed
    // Print-once means a marked-but-unprinted block can never be emitted again
    terminal.insert_before_rows(&rows)?;
    insert_gap(terminal);
    Ok(())
}

/// Emit [`MINIMAL_BLOCK_GAP`] blank rows into native scrollback as the trailing gap after a committed block.
/// The rows are left unpainted, so they inherit the terminal's own background (matching the flat, transparent committed look).
pub(super) fn insert_gap(terminal: &mut PagerTerminal) {
    if MINIMAL_BLOCK_GAP == 0 {
        return;
    }
    let _ = terminal.insert_before(MINIMAL_BLOCK_GAP, |_buf| {});
}

/// Paint a committed block into `buf` (a `commit_h`-row buffer), laying it out at its full `full_h` so wrapping matches an uncapped commit.
/// When `buf` is shorter than `full_h` the block is capped: rows past it are clipped and the final row becomes the `… N more lines` footer.
/// Extracted from [`insert_committed`] so the cap is unit-testable without a live terminal.
fn paint_committed(
    buf: &mut ratatui::buffer::Buffer,
    renderer: &EntryRenderer<'_>,
    width: u16,
    full_h: u16,
    footer_style: Style,
) {
    let commit_h = buf.area.height;
    let area = Rect {
        x: buf.area.x,
        y: buf.area.y,
        width,
        height: full_h,
    };
    renderer.render(area, buf);
    if commit_h > 0 && commit_h < full_h {
        // Top `commit_h - 1` rows are content; the last row is the footer.
        let hidden = full_h.saturating_sub(commit_h.saturating_sub(1));
        let y = buf.area.y + commit_h - 1;
        let row = Rect {
            x: buf.area.x,
            y,
            width,
            height: 1,
        };
        // The footer uses dim default-fg chrome (not hard-coded DarkGray) so it follows the terminal-native palette
        let style = footer_style.bg(Color::Reset);
        // Clear any clipped content that landed on the footer row first.
        buf.set_style(row, style);
        let text = format!("\u{2026} {hidden} more lines \u{00b7} /transcript to view");
        buf.set_span(buf.area.x, y, &Span::styled(text, style), width);
    }
    super::full_view::trim_trailing_pads(buf);
}

/// Renderer joiners for the painted commit. A cap footer is not a wrap continuation.
fn commit_wrap_flags(renderer: &EntryRenderer<'_>, height: u16, full_h: u16) -> Vec<bool> {
    let mut wraps = renderer.row_soft_wraps(height);
    if height > 0 && height < full_h {
        let last = usize::from(height) - 1;
        if let Some(flag) = wraps.get_mut(last) {
            *flag = false;
        }
        if let Some(flag) = last.checked_sub(1).and_then(|i| wraps.get_mut(i)) {
            *flag = false;
        }
    }
    wraps
}

/// Commit the active agent's newly-finalized blocks into native scrollback. Minimal has no separate history pane,
/// so the terminal's scrollback *is* the history. a resumed session would otherwise look empty.
pub fn commit_active(app: &mut AppView, terminal: &mut PagerTerminal) {
    let id = match &app.active_view {
        ActiveView::Agent(id) => *id,
        _ => return, // welcome / dashboard: nothing to commit
    };
    // Snapshot the commit appearance before borrowing `agents` mutably.
    let appearance = committed_appearance(&app.appearance);
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };
    // Hold commits while a band-owning modal (settings, palette, feedback form) is open
    // It takes the whole live region, so an `insert_before` underneath it would scroll the popup
    // Deferred commits flush on the next frame after it closes
    if super::overlay::is_live_region_modal_active(agent) {
        return;
    }
    // The sizing pass and this commit pass must judge committability against the same marks. Syncing here would let a
    // tool look committable to `sync_viewport`/`tail_height` on the very frame its permission arrived.
    let turn_running = minimal_api::is_turn_or_wake_running(agent);
    let cwd = agent.session.cwd.as_path();
    let sb = &mut agent.scrollback;

    // NB: resume/attach replay (`agent.session.loading_replay`) intentionally falls through to the normal commit pass below
    // The loaded transcript prints into native scrollback; a resumed session must be visible

    let theme = Theme::current();
    let footer_style = theme.dim();
    let max_rows = appearance.minimal_max_commit_rows;
    let width = terminal.viewport_area().width;
    if width == 0 {
        return;
    }

    // Drive the ONE frontier walk (`commit_leading_run`, also what the unit tests exercise) with the production per-entry work
    // The per-entry work finalizes, stamps the print-once display mode, prints, then remembers folded blocks for Ctrl+E
    commit_leading_run(sb, turn_running, |sb, i| {
        // If the turn is idle but this entry still carries a stale `is_running` flag, finalize it first
        // It then renders in its finished form (e.g. "Thought for Xs", not an animated "Thinking…").
        if let Some(id) = sb.get(i).filter(|e| e.is_running).map(|e| e.id) {
            sb.finish_running(id);
        }
        // Stamp the print-once display mode before measuring/rendering.
        if let Some(e) = sb.get_mut(i) {
            let mode = minimal_commit_display_mode(&e.block, &appearance);
            e.set_display_mode(mode);
        }
        if let Some(e) = sb.get(i) {
            // A failed write returns `false` so the walk leaves the entry uncommitted. (retried next frame) instead of marking
            // a never-printed block. NOTE (print-once contract): from a successful insert on, the entry's content is frozen on
            // the user's terminal.
            let renderer = minimal_renderer(e, &theme, appearance.clone(), cwd, COMMITTED_TICK);
            if insert_committed(terminal, renderer, width, max_rows, footer_style).is_err() {
                return false;
            }
        }
        // Remember folded blocks (collapsed reasoning / truncated output) so `Ctrl+E` / `/expand` can re-print them in full later
        // They are recorded only after the print actually succeeded
        if let Some((id, mode)) = sb.get(i).map(|e| (e.id, e.display_mode()))
            && matches!(mode, DisplayMode::Collapsed | DisplayMode::Truncated)
        {
            minimal_api::record_committed_for_expand(sb, id);
        }
        true
    });

    // Otherwise the live region is tall while a block streams and snaps short the instant it finalizes, jerking the
    // prompt upward.
    let mut j = minimal_api::commit_scan_cursor(sb);
    while let Some(e) = sb.get_mut(j) {
        let mode = minimal_commit_display_mode(&e.block, &appearance);
        e.set_display_mode(mode);
        j += 1;
    }
}

/// Committed terminal text cannot be mutated in place, so "expanding" a folded block is an honest re-print of the
/// same entry in `Expanded` mode. Capping the explicit "show me the whole thing" action would just reprint the same
/// footer.
pub fn expand_pending(app: &mut AppView, terminal: &mut PagerTerminal) {
    if minimal_api::minimal_pending_expand(app).is_empty() {
        return;
    }
    let id = match &app.active_view {
        ActiveView::Agent(id) => *id,
        _ => return,
    };
    let width = terminal.viewport_area().width;
    if width == 0 {
        return;
    }
    let appearance = committed_appearance(&app.appearance);
    // Guards: a missing active agent must leave the IDs queued, so confirm it exists before consuming the queue below.
    // The queue take needs `&mut app`, which can't overlap the agent borrow, hence the check-then-reborrow. An
    // `insert_before` would scroll the popup or the feedback form and the user wouldn't see the re-print.
    match app.agents.get(&id) {
        Some(agent) if !super::overlay::is_live_region_modal_active(agent) => {}
        _ => return,
    }
    let theme = Theme::current();
    let footer_style = theme.dim();
    // Consume the expand queue only after every guard above has passed
    // A non-agent active view, a 0-width (probe) frame, or an open band-owning modal must leave the IDs queued for a later frame
    let ids = minimal_api::take_minimal_pending_expand(app);
    let mut requeue: Vec<EntryId> = Vec::new();
    {
        let Some(agent) = app.agents.get_mut(&id) else {
            // Can't happen (existence checked just above, nothing in between can remove the agent)
            // If it ever does, the drained queue must go back rather than silently vanish
            minimal_api::requeue_minimal_pending_expand(app, ids);
            return;
        };
        let cwd = agent.session.cwd.as_path();
        let sb = &mut agent.scrollback;
        let mut iter = ids.into_iter();
        while let Some(eid) = iter.next() {
            let Some(idx) = sb.index_of_id(eid) else {
                continue; // entry removed (rewind / clear) since the keypress
            };
            if let Some(e) = sb.get_mut(idx) {
                e.set_display_mode(DisplayMode::Expanded);
            }
            if let Some(e) = sb.get(idx) {
                let renderer = minimal_renderer(e, &theme, appearance.clone(), cwd, COMMITTED_TICK);
                if insert_committed(terminal, renderer, width, 0, footer_style).is_err() {
                    // Terminal write failed: keep this id and the rest queued so the request retries next frame instead of vanishing
                    requeue.push(eid);
                    requeue.extend(iter);
                    break;
                }
            }
        }
    }
    if !requeue.is_empty() {
        minimal_api::requeue_minimal_pending_expand(app, requeue);
    }
}

/// Re-mark tool entries that are blocked on a pending permission/question so the frontier holds them in the live
/// region. The viewport sizing, the `will_commit` gate, and the commit pass must all judge committability against
/// the same marks.
pub fn sync_pending_marks(app: &mut AppView) {
    if let ActiveView::Agent(id) = &app.active_view
        && let Some(agent) = app.agents.get_mut(id)
    {
        minimal_api::sync_pending_user_input_marks(agent);
    }
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
