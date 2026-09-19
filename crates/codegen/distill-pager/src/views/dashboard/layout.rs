//! Pure layout computation for the dashboard view.
//!
//! How the peek and the roster split the vertical space is specified in
//! [`docs/internal/33-dashboard-peek-responsive-layout.md`](../../../../docs/internal/33-dashboard-peek-responsive-layout.md).

use ratatui::layout::Rect;

/// Minimum width at which the dashboard can render meaningful rows.
/// Below this, the renderer falls back to a stripped, single-column view; row labels are middle-truncated.
pub const MIN_DASHBOARD_WIDTH: u16 = 40;

/// Minimum list-band height (terminal rows) while evaluating or opening a peek.
pub const LIST_FLOOR_ROWS: u16 = 12;

/// Minimum height of the whole peek box (borders, status, body, and reply) for a live-tail peek.
pub const PEEK_MIN_BOX_LIVE_TAIL: u16 = 8;

/// Minimum height of the whole peek box for question and permission peeks (options need room).
pub const PEEK_MIN_BOX_QUESTION: u16 = 10;

/// The peek max is ⌊H × PEEK_MAX_FRAC_NUM / PEEK_MAX_FRAC_DEN⌋ (whole box).
pub const PEEK_MAX_FRAC_NUM: u16 = 3;
pub const PEEK_MAX_FRAC_DEN: u16 = 8;

/// Secondary cap on live-tail body rows inside an allocated peek box.
pub const MAX_LIVE_TAIL_ROWS: u16 = 28;

/// Live-tail height budget for a peek with no question (status, optional blank, and reply rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeekLiveTailBudget {
    pub live_tail: u16,
    pub blank_row: bool,
    pub content_rows: u16,
}

/// Result of list-first peek allocation for height `H`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeekAllocation {
    pub show_peek: bool,
    /// Whole peek box height including borders; 0 if `!show_peek`.
    pub peek_box_h: u16,
    /// Max inner content rows for a peek at full allowed size (`peek_box_h - 2`).
    pub max_content_rows: u16,
}

/// The desired inner rows for a live-tail peek, shrunk to its content. (The paint blanks only when
/// the middle still has 2 or more rows after the blank, so the pin and body share.). The result
/// never exceeds `max_content`; the body is also capped by [`MAX_LIVE_TAIL_ROWS`].
pub fn peek_live_tail_desired_content(
    max_content: u16,
    reply_rows: u16,
    body_measured: u16,
    pin_user: bool,
) -> PeekLiveTailBudget {
    let reply_rows = reply_rows.max(1);
    let pin = u16::from(pin_user);
    let fixed = 1u16 + reply_rows + pin; // status + reply + optional pin

    if max_content < fixed {
        return PeekLiveTailBudget {
            live_tail: 0,
            blank_row: false,
            content_rows: max_content,
        };
    }

    let room_no_blank = max_content.saturating_sub(fixed).min(MAX_LIVE_TAIL_ROWS);
    if room_no_blank == 0 {
        return PeekLiveTailBudget {
            live_tail: 0,
            blank_row: false,
            content_rows: fixed,
        };
    }

    // Prefer a breathing blank whenever body is non-empty and room remains.
    let room_with_blank = max_content
        .saturating_sub(fixed + 1)
        .min(MAX_LIVE_TAIL_ROWS);
    let blank = room_with_blank > 0;
    let body_cap = if blank {
        room_with_blank
    } else {
        room_no_blank
    };

    let body = if body_measured == 0 {
        1u16.min(body_cap)
    } else {
        body_measured.min(body_cap)
    };
    let blank = blank && body > 0;
    let content_rows = fixed + u16::from(blank) + body;
    PeekLiveTailBudget {
        live_tail: body,
        blank_row: blank,
        content_rows: content_rows.min(max_content),
    }
}

/// The whole-box peek cap: ⌊H × 3/8⌋ rows.
pub fn peek_max_box_rows(h: u16) -> u16 {
    ((u32::from(h) * u32::from(PEEK_MAX_FRAC_NUM)) / u32::from(PEEK_MAX_FRAC_DEN)) as u16
}

/// Chrome rows (header, gaps, footer, margins); the list and peek are not counted.
pub fn chrome_overhead(area: Rect) -> u16 {
    dashboard_chrome_heights(area).total()
}

/// List-first peek allocation. Otherwise the peek height is `min(desired_content+2,
/// max_candidate)`, at least `peek_min_box` when showing. A growing reply should increase it; the
/// list may shrink only down to the floor (the max candidate enforces this).
pub fn allocate_peek(
    area_h: u16,
    fixed_overhead: u16,
    desired_content_rows: u16,
    peek_min_box: u16,
) -> PeekAllocation {
    let after = area_h.saturating_sub(fixed_overhead);
    if after == 0 {
        return PeekAllocation {
            show_peek: false,
            peek_box_h: 0,
            max_content_rows: 0,
        };
    }
    let list_floor = LIST_FLOOR_ROWS.min(after);
    let remainder = after.saturating_sub(list_floor);
    let peek_max = peek_max_box_rows(area_h);
    let max_peek = remainder.min(peek_max);
    let max_content_rows = max_peek.saturating_sub(2);

    if max_peek < peek_min_box {
        return PeekAllocation {
            show_peek: false,
            peek_box_h: 0,
            max_content_rows,
        };
    }

    let desired_box = desired_content_rows.saturating_add(2);
    let peek_box_h = desired_box.max(peek_min_box).min(max_peek);

    PeekAllocation {
        show_peek: true,
        peek_box_h,
        max_content_rows,
    }
}

/// Outer horizontal padding for the dispatch box (cols on each side).
///
/// Matches `LayoutConfig::outer_hpad_left/right = 2` from the agent view's default appearance config.
pub const DISPATCH_OUTER_HPAD: u16 = 2;

/// Outer horizontal padding for the top page header (cols on each side).
/// Matches list/dispatch so the title aligns with content below.
pub const HEADER_OUTER_HPAD: u16 = 2;

/// Outer horizontal padding for the row list (cols on each side). Gives the list (rows, group
/// headers, scrollbar) breathing room. Selection markers, group header rules (` `), and row text
/// don't sit flush against the terminal edges.
pub const LIST_OUTER_HPAD: u16 = 2;

/// Output of [`compute_layout`].
#[derive(Debug, Clone, Copy)]
pub struct DashboardLayout {
    /// Top margin row (blank space above the header). Height: 0 or 1.
    /// Matches the welcome view's `v_margin` so the dashboard's title row doesn't sit flush against the alt-screen top edge.
    pub top_margin: Rect,
    /// Header row (location label + state chips). Height: 0 or 1.
    pub header: Rect,
    /// Vertical breathing room between the header and the row list. Height: 0 or 1. It is a named rect
    /// rather than an anonymous y-cursor bump only so tests can pin its position and threshold.
    pub header_gap: Rect,
    /// Primary actions row (`+ New Agent` on the left, `Open Previous | Worktree` on the right). Height: 0 or 1, same threshold as `header`.
    pub actions: Rect,
    /// Vertical breathing room between the actions row and the row list. Height: 0 or 1; drops to 0 at `area.height <= 13`.
    pub actions_gap: Rect,
    /// Scrollable list area (rows + group headers).
    pub list: Rect,
    /// Bottom dispatch input area.
    pub dispatch: Rect,
    /// Footer / shortcut hint row.
    pub footer: Rect,
    /// Bottom margin row (blank space below the shortcuts bar). Height: 0 or 1.
    /// Matches the agent view's `bottom_vpad` so the shortcuts bar doesn't sit flush against the alt-screen's bottom edge.
    /// Drops to 0 on short terminals (`area.height <= 16`, the same threshold as `views::agent::AgentViewLayout::compute`).
    pub bottom_margin: Rect,
}

/// `peek_visible` requests the peek panel; the layout shows it only when the area has enough
/// vertical room.
pub fn compute_layout(area: Rect, peek_visible: bool) -> DashboardLayout {
    // A single text row is the default
    // Callers that support a growing multiline dispatch box (Shift+Enter newlines) use [`compute_layout_with_dispatch`] to request more
    compute_layout_with_dispatch(area, peek_visible, 1)
}

/// Heights (0 or 1) of the fixed chrome rows; the list and the dispatch box are sized from what remains.
struct ChromeHeights {
    top_margin: u16,
    header: u16,
    header_gap: u16,
    actions: u16,
    actions_gap: u16,
    footer: u16,
    dispatch_gap: u16,
    shortcuts_gap: u16,
    bottom_margin: u16,
    short_terminal: bool,
}

impl ChromeHeights {
    fn total(&self) -> u16 {
        self.top_margin
            + self.header
            + self.header_gap
            + self.actions
            + self.actions_gap
            + self.footer
            + self.dispatch_gap
            + self.shortcuts_gap
            + self.bottom_margin
    }
}

fn dashboard_chrome_heights(area: Rect) -> ChromeHeights {
    // Match the welcome and agent top margins; drop on short terminals
    let top_margin = u16::from(area.height > 6);
    let header = u16::from(area.height > 4);
    // The actions row carries the `+ New Agent` cursor target, so it survives as long as the header does
    let actions = header;
    // The four blank gaps switch on one height apart, bottom to top, so each extra terminal row goes to the list or to exactly one
    // gap: from height 9 up (past the short-terminal dispatch boundary) the list never shrinks as the terminal grows
    // (see `layout_tests::layout_list_height_never_shrinks_as_terminal_grows`)
    let dispatch_gap = u16::from(area.height > 10);
    let shortcuts_gap = u16::from(area.height > 11);
    let header_gap = u16::from(area.height > 12);
    let actions_gap = u16::from(area.height > 13);
    let footer = u16::from(area.height >= 2);
    // Match agent bottom_vpad; drop when height <= 16.
    let bottom_margin = u16::from(area.height > 16);
    ChromeHeights {
        top_margin,
        header,
        header_gap,
        actions,
        actions_gap,
        footer,
        dispatch_gap,
        shortcuts_gap,
        bottom_margin,
        short_terminal: area.height <= 8,
    }
}

/// Max inner content rows available for a peek under list-first allocation (list floor and peek max fraction).
/// Returns 0 when a peek cannot open.
pub fn max_peek_content_rows(area: Rect) -> u16 {
    if area.height <= 8 {
        return 0;
    }
    let fixed = chrome_overhead(area);
    let probe = allocate_peek(
        area.height,
        fixed,
        // Probe with enough content that allocation uses full max candidate.
        255,
        PEEK_MIN_BOX_LIVE_TAIL,
    );
    probe.max_content_rows
}

/// Like [`compute_layout`] but with a fixed whole peek-box height (from [`allocate_peek`]).
/// The list band receives the rest after chrome.
pub fn compute_layout_with_peek_box(area: Rect, peek_box_h: u16) -> DashboardLayout {
    compute_layout_with_dispatch_inner(area, true, 0, Some(peek_box_h.max(3)))
}

/// Like [`compute_layout`] but lets the caller request a taller dispatch box. `dispatch_text_rows`
/// is the number of text rows the dispatch input wants (at least 1); the box adds 2 more for its
/// top and bottom borders.
pub fn compute_layout_with_dispatch(
    area: Rect,
    peek_visible: bool,
    dispatch_text_rows: u16,
) -> DashboardLayout {
    compute_layout_with_dispatch_inner(area, peek_visible, dispatch_text_rows, None)
}

fn compute_layout_with_dispatch_inner(
    area: Rect,
    peek_visible: bool,
    dispatch_text_rows: u16,
    forced_peek_box_h: Option<u16>,
) -> DashboardLayout {
    // When `area.height == 0`, every subrect collapses to zero
    // A default `footer_h` of 1 would otherwise produce a non-zero footer rect even on a 0-height area
    if area.height == 0 {
        let z = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 0,
        };
        return DashboardLayout {
            top_margin: z,
            header: z,
            header_gap: z,
            actions: z,
            actions_gap: z,
            list: z,
            dispatch: z,
            footer: z,
            bottom_margin: z,
        };
    }
    let chrome = dashboard_chrome_heights(area);
    let fixed_overhead = chrome.total();
    let short_terminal = chrome.short_terminal;

    // Peek: list-first allocation (see `allocate_peek`). With no peek, normal dispatch chrome applies.
    // `forced_peek_box_h` skips re-allocation when the caller already chose a height (and the peek min for question vs live-tail)
    let dispatch_h: u16 = if let Some(h) = forced_peek_box_h {
        let after = area.height.saturating_sub(fixed_overhead);
        let list_floor = LIST_FLOOR_ROWS.min(after);
        let max_peek = after
            .saturating_sub(list_floor)
            .min(peek_max_box_rows(area.height));
        h.min(max_peek).max(3)
    } else if peek_visible {
        if short_terminal {
            1
        } else {
            let alloc = allocate_peek(
                area.height,
                fixed_overhead,
                dispatch_text_rows,
                PEEK_MIN_BOX_LIVE_TAIL,
            );
            if alloc.show_peek { alloc.peek_box_h } else { 3 }
        }
    } else if !short_terminal {
        2 + dispatch_text_rows.max(1)
    } else {
        1
    };
    // A multiline draft may ask for more rows than the chrome leaves; the list gives way first, but the box must still end inside `area` or
    // the footer would be painted past the buffer
    let dispatch_h = dispatch_h.min(area.height.saturating_sub(fixed_overhead));
    // The peek renders inside the dispatch rect (which grows when `peek_visible`, computed above); there is no standalone peek rect
    let remaining = area.height.saturating_sub(fixed_overhead + dispatch_h);

    let mut y = area.y;
    let top_margin = Rect {
        x: area.x,
        y,
        width: area.width,
        height: chrome.top_margin,
    };
    y += chrome.top_margin;
    // Inset the top page header (and the actions row below it) to match the list and dispatch content columns
    let header_inner_pad = HEADER_OUTER_HPAD.saturating_mul(2);
    let header_width = area.width.saturating_sub(header_inner_pad);
    let header_x = if header_width > 0 {
        area.x.saturating_add(HEADER_OUTER_HPAD)
    } else {
        area.x
    };
    let header_width = if header_width > 0 {
        header_width
    } else {
        area.width
    };
    let header = Rect {
        x: header_x,
        y,
        width: header_width,
        height: chrome.header,
    };
    y += chrome.header;

    // 1-row gap between the header and the actions row (collapsed on short terminals)
    // Painted by `render_dashboard`'s full-area fill; no sub-renderer touches it
    let header_gap = Rect {
        x: area.x,
        y,
        width: area.width,
        height: chrome.header_gap,
    };
    y += chrome.header_gap;

    let actions = Rect {
        x: header_x,
        y,
        width: header_width,
        height: chrome.actions,
    };
    y += chrome.actions;

    let actions_gap = Rect {
        x: area.x,
        y,
        width: area.width,
        height: chrome.actions_gap,
    };
    y += chrome.actions_gap;

    // Inset the list by LIST_OUTER_HPAD on each side so the row content and group header rules have side breathing room
    // The outer columns stay painted bg_base by the area-wide fill in render_dashboard
    // Mirrors the dispatch inset pattern but with a smaller pad (1 vs 2) because row text is long and dense
    let list_inner_pad = LIST_OUTER_HPAD.saturating_mul(2);
    let list_width = area.width.saturating_sub(list_inner_pad);
    let list_x = if list_width > 0 {
        area.x.saturating_add(LIST_OUTER_HPAD)
    } else {
        area.x
    };
    let list = Rect {
        x: list_x,
        y,
        width: if list_width > 0 {
            list_width
        } else {
            area.width
        },
        height: remaining,
    };
    y += remaining;

    // 1-row gap between the list (or peek) and the dispatch box (mirrors `prompt_gap` in `views::agent::AgentViewLayout`)
    y += chrome.dispatch_gap;

    // The single-line dispatch input keeps its 2-col outer padding so the `❯` prefix lines up with the row content
    // Rows are indented past the marker column too
    let dispatch_inner_pad = DISPATCH_OUTER_HPAD.saturating_mul(2);
    let dispatch_width = area.width.saturating_sub(dispatch_inner_pad);
    let dispatch_x = if dispatch_width > 0 {
        area.x.saturating_add(DISPATCH_OUTER_HPAD)
    } else {
        area.x
    };
    let dispatch = Rect {
        x: dispatch_x,
        y,
        width: if dispatch_width > 0 {
            dispatch_width
        } else {
            area.width
        },
        height: dispatch_h,
    };
    y += dispatch_h;

    // 1-row gap between the dispatch box and the shortcuts footer (mirrors `shortcuts_gap`)
    y += chrome.shortcuts_gap;

    let footer = Rect {
        x: area.x,
        y,
        width: area.width,
        height: chrome.footer,
    };
    y += chrome.footer;

    // Bottom margin row below the shortcuts bar (matches the agent view's `bottom_vpad`)
    // Painted with `bg_base` by `render_dashboard`'s full-area fill; no sub-renderer ever touches this rect
    let bottom_margin = Rect {
        x: area.x,
        y,
        width: area.width,
        height: chrome.bottom_margin,
    };

    DashboardLayout {
        top_margin,
        header,
        header_gap,
        actions,
        actions_gap,
        list,
        dispatch,
        footer,
        bottom_margin,
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
