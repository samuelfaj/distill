use super::*;

#[test]
fn layout_assigns_disjoint_areas() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, true);
    // Four blank gap rows sit between the content rects: header_gap, actions_gap, dispatch_gap, and shortcuts_gap
    // The bottom_margin is a real rect painted bg_base, so it counts inside the total
    // The gaps are intentional blank breathing-room rows that the area-wide bg fill paints without any dedicated sub-renderer
    let total = layout.top_margin.height
        + layout.header.height
        + layout.actions.height
        + layout.list.height
        + layout.dispatch.height
        + layout.footer.height
        + layout.bottom_margin.height;
    // The 4 rows absorbed by the gaps: header_gap, actions_gap, dispatch_gap, and shortcuts_gap
    assert_eq!(total + 4, area.height);
}

/// The actions row sits one blank row below the header and one blank row above the list, sharing the header's horizontal inset.
#[test]
fn layout_places_actions_row_between_header_and_list() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(layout.actions.height, 1);
    assert_eq!(
        layout.actions.y,
        layout.header.y + 2,
        "one blank row between header and actions"
    );
    assert_eq!(layout.actions_gap.height, 1);
    assert_eq!(layout.actions_gap.y, layout.actions.y + 1);
    assert_eq!(
        layout.list.y,
        layout.actions.y + 2,
        "one blank row between actions and list"
    );
    assert_eq!(layout.actions.x, layout.header.x);
    assert_eq!(layout.actions.width, layout.header.width);
}

/// On short terminals the gaps around the actions row collapse, but the row itself stays as long as the header does.
/// The four gaps switch on one height apart, bottom to top: dispatch gap at 11, shortcuts gap at 12, header gap at 13, actions gap at 14.
#[test]
fn layout_actions_row_follows_header_on_short_terminals() {
    for h in [10u16, 11, 12] {
        let short = compute_layout(Rect::new(0, 0, 80, h), false);
        assert_eq!(short.header.height, 1, "h={h}");
        assert_eq!(short.actions.height, 1, "h={h}");
        assert_eq!(short.header_gap.height, 0, "h={h}");
        assert_eq!(short.actions_gap.height, 0, "h={h}");
        assert_eq!(short.actions.y, short.header.y + 1, "h={h}");
        assert_eq!(short.list.y, short.actions.y + 1, "h={h}");
    }
    let gaps = |h: u16| {
        let l = compute_layout(Rect::new(0, 0, 80, h), false);
        [
            l.dispatch.y - (l.list.y + l.list.height), // dispatch gap
            l.footer.y - (l.dispatch.y + l.dispatch.height), // shortcuts gap
            l.header_gap.height,
            l.actions_gap.height,
        ]
    };
    assert_eq!(gaps(10), [0, 0, 0, 0]);
    assert_eq!(gaps(11), [1, 0, 0, 0]);
    assert_eq!(gaps(12), [1, 1, 0, 0]);
    assert_eq!(gaps(13), [1, 1, 1, 0]);
    assert_eq!(gaps(14), [1, 1, 1, 1]);

    let tiny = compute_layout(Rect::new(0, 0, 80, 4), false);
    assert_eq!(tiny.header.height, 0);
    assert_eq!(tiny.actions.height, 0);
}

/// Growing the terminal must never hide list content: with the default one-row draft the list keeps at least two rows from height 9
/// up, and its height is non-decreasing in the terminal height, including across every gap threshold and the bottom-margin step.
#[test]
fn layout_list_height_never_shrinks_as_terminal_grows() {
    let list_h = |h: u16| {
        compute_layout_with_dispatch(Rect::new(0, 0, 80, h), false, 1)
            .list
            .height
    };
    let mut prev = list_h(9);
    assert!(
        prev >= 2,
        "h=9: list must keep at least two rows, got {prev}"
    );
    for h in 10u16..=60 {
        let cur = list_h(h);
        assert!(
            cur >= prev,
            "h={h}: list shrank from {prev} to {cur} as the terminal grew",
        );
        prev = cur;
    }
    // The band the gaps switch on in, pinned so a threshold edit shows up here
    assert_eq!(
        (9..=18).map(list_h).collect::<Vec<_>>(),
        [2, 3, 3, 3, 3, 3, 4, 5, 5, 6]
    );
}

/// Every rect ends inside the area at every height, whatever a multiline draft asks for.
/// Otherwise the footer would be painted past the buffer and `ShortcutsBar` would panic.
/// The sweep runs past the renderer's own draft cap on purpose: the clamp is a layout invariant, not renderer policy.
#[test]
fn layout_never_overflows_area_for_any_dispatch_rows() {
    for h in 1u16..=40 {
        let area = Rect::new(0, 0, 80, h);
        for text_rows in 1..=h.saturating_add(4) {
            let layout = compute_layout_with_dispatch(area, false, text_rows);
            for (name, rect) in [
                ("top_margin", layout.top_margin),
                ("header", layout.header),
                ("header_gap", layout.header_gap),
                ("actions", layout.actions),
                ("actions_gap", layout.actions_gap),
                ("list", layout.list),
                ("dispatch", layout.dispatch),
                ("footer", layout.footer),
                ("bottom_margin", layout.bottom_margin),
            ] {
                assert!(
                    rect.y + rect.height <= area.y + area.height,
                    "h={h} text_rows={text_rows}: {name} ends at {} past the area bottom {}",
                    rect.y + rect.height,
                    area.y + area.height,
                );
            }
            assert_eq!(
                layout.footer.y + layout.footer.height + layout.bottom_margin.height,
                area.y + area.height,
                "h={h} text_rows={text_rows}: the footer must sit on the last row (above any bottom margin)",
            );
        }
    }
}

/// Multiline dispatch: the box grows by exactly one row per extra text row (2 border rows plus N text rows).
/// The row list gives up the space so the totals still tile the area.
#[test]
fn dispatch_box_grows_for_multiline_input() {
    let area = Rect::new(0, 0, 80, 30);
    let single = compute_layout(area, false);
    // The single-line default is 3 rows: top border, 1 text row, bottom border
    assert_eq!(single.dispatch.height, 3);

    let three = compute_layout_with_dispatch(area, false, 3);
    assert_eq!(
        three.dispatch.height, 5,
        "3 text rows → 2 border + 3 text = 5 rows",
    );
    // The list absorbs the extra two rows the dispatch box took.
    assert_eq!(
        three.list.height + 2,
        single.list.height,
        "the row list must shrink by exactly the dispatch growth",
    );
    // Dispatch still sits above the footer with the same gap.
    assert_eq!(three.footer.y, three.dispatch.y + three.dispatch.height + 1);
}

/// A `dispatch_text_rows` of 0 is floored to a single text row so the box never collapses below its single-line chrome.
#[test]
fn dispatch_box_floors_at_single_text_row() {
    let area = Rect::new(0, 0, 80, 30);
    let zero = compute_layout_with_dispatch(area, false, 0);
    assert_eq!(zero.dispatch.height, 3, "0 text rows floors to 1 (3 total)");
}

/// The dashboard reserves 1 row of bottom margin below the shortcuts bar on tall enough terminals.
/// Without it the shortcuts sit flush against the alt-screen's bottom edge.
/// Mirrors the agent view's `bottom_vpad` (`outer_vpad = 1`, dropped to 0 at `area.height <= 16`).
#[test]
fn layout_reserves_bottom_margin_on_tall_terminals() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.bottom_margin.height, 1,
        "tall terminal must reserve a bottom margin row",
    );
    assert_eq!(
        layout.bottom_margin.y,
        layout.footer.y + layout.footer.height,
        "bottom_margin must sit directly below the footer",
    );
    assert_eq!(
        layout.bottom_margin.y + layout.bottom_margin.height,
        area.y + area.height,
        "bottom_margin must extend to the bottom of `area`",
    );
}

/// Bottom margin collapses to 0 on short terminals (`area.height <= 16`, matching the agent view's threshold) so the row list isn't starved.
#[test]
fn layout_drops_bottom_margin_on_short_terminals() {
    let area = Rect::new(0, 0, 80, 16);
    let layout = compute_layout(area, false);
    assert_eq!(layout.bottom_margin.height, 0);
}

/// The dispatch box gets `DISPATCH_OUTER_HPAD` cols of outer padding on each side so its rounded border doesn't reach the terminal edge.
/// Matches the agent view's `outer_hpad_left/right`.
#[test]
fn layout_applies_outer_hpad_to_dispatch_box() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.dispatch.x,
        area.x + DISPATCH_OUTER_HPAD,
        "dispatch must be inset by DISPATCH_OUTER_HPAD on the left",
    );
    assert_eq!(
        layout.dispatch.width,
        area.width - DISPATCH_OUTER_HPAD * 2,
        "dispatch width must lose DISPATCH_OUTER_HPAD on each side",
    );
}

/// Header is inset by HEADER_OUTER_HPAD; list by LIST_OUTER_HPAD.
/// Footer remains full-width.
#[test]
fn layout_insets_header_and_list() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.header.width,
        area.width - HEADER_OUTER_HPAD * 2,
        "header must be inset by HEADER_OUTER_HPAD on each side",
    );
    assert_eq!(layout.footer.width, area.width);
    assert_eq!(layout.list.width, area.width - LIST_OUTER_HPAD * 2);
}

/// The list rect is inset by LIST_OUTER_HPAD cols on each side so row content (markers, rules, text) doesn't touch the terminal edges.
/// The outer columns remain bg_base (painted by the top-level area fill).
#[test]
fn layout_applies_outer_hpad_to_list() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.list.x,
        area.x + LIST_OUTER_HPAD,
        "list must be inset by LIST_OUTER_HPAD on the left",
    );
    assert_eq!(
        layout.list.width,
        area.width - LIST_OUTER_HPAD * 2,
        "list width must lose LIST_OUTER_HPAD on each side",
    );
}

/// The header's horizontal pad matches the list's so the title aligns with the content columns.
#[test]
fn layout_applies_outer_hpad_to_header() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(HEADER_OUTER_HPAD, LIST_OUTER_HPAD);
    assert_eq!(
        layout.header.x,
        area.x + HEADER_OUTER_HPAD,
        "header must be inset by HEADER_OUTER_HPAD on the left",
    );
    assert_eq!(
        layout.header.width,
        area.width - HEADER_OUTER_HPAD * 2,
        "header width must lose HEADER_OUTER_HPAD on each side",
    );
    assert_eq!(layout.header.x, layout.list.x);
    assert_eq!(layout.header.width, layout.list.width);
}

/// A 1-row gap separates the list (or peek) from the dispatch box, and another 1-row gap separates the dispatch box from the footer.
/// Mirrors `prompt_gap` and `shortcuts_gap` in the agent view's layout.
#[test]
fn layout_reserves_dispatch_and_shortcuts_gaps() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    // 1-row gap before dispatch
    let list_end = layout.list.y + layout.list.height;
    assert_eq!(
        layout.dispatch.y - list_end,
        1,
        "expected 1-row gap between list and dispatch, got {} (list_end={list_end}, dispatch.y={})",
        layout.dispatch.y - list_end,
        layout.dispatch.y,
    );
    // 1-row gap before footer
    let dispatch_end = layout.dispatch.y + layout.dispatch.height;
    assert_eq!(
        layout.footer.y - dispatch_end,
        1,
        "expected 1-row gap between dispatch and footer, got {} (dispatch_end={dispatch_end}, footer.y={})",
        layout.footer.y - dispatch_end,
        layout.footer.y,
    );
}

/// Gaps collapse to 0 on short terminals so the row list isn't starved.
/// The dispatch gap switches on at `height > 10` and the shortcuts gap at `height > 11`; at 10 both are still off.
#[test]
fn layout_drops_gaps_on_short_terminals() {
    let area = Rect::new(0, 0, 80, 10);
    let layout = compute_layout(area, false);
    let list_end = layout.list.y + layout.list.height;
    let dispatch_end = layout.dispatch.y + layout.dispatch.height;
    assert_eq!(
        layout.dispatch.y - list_end,
        0,
        "short terminal must collapse dispatch gap",
    );
    assert_eq!(
        layout.footer.y - dispatch_end,
        0,
        "short terminal must collapse shortcuts gap",
    );
}

/// The dashboard reserves a 1-row gap between the header and the row list on tall enough terminals.
/// The status chips and `Dashboard` label then don't sit flush against the first group header or row.
/// The gap sits immediately below the header and immediately above the list.
#[test]
fn layout_reserves_header_gap_on_tall_terminals() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.header_gap.height, 1,
        "tall terminal must reserve a header gap row",
    );
    assert_eq!(
        layout.header_gap.y,
        layout.header.y + layout.header.height,
        "header_gap must sit directly below the header",
    );
    assert_eq!(
        layout.actions.y,
        layout.header_gap.y + layout.header_gap.height,
        "the actions row must start directly below the header_gap",
    );
}

/// The header gap collapses to 0 on short terminals (`area.height <= 12`; it is the third of the four gaps to switch on).
/// This keeps the row list from being starved.
#[test]
fn layout_drops_header_gap_on_short_terminals() {
    let area = Rect::new(0, 0, 80, 10);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.header_gap.height, 0,
        "short terminal must collapse header_gap",
    );
    assert_eq!(
        layout.actions.y,
        layout.header.y + layout.header.height,
        "the actions row must start directly below the header when the gap collapses",
    );
}

/// The dashboard reserves one row of top margin on terminals tall enough to spare it, mirroring the welcome view's `v_margin`.
/// Below the height threshold the margin collapses to 0 so the row list isn't starved.
#[test]
fn layout_reserves_top_margin_on_tall_terminals() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.top_margin.height, 1,
        "tall terminal must reserve a top margin row",
    );
    assert_eq!(
        layout.header.y,
        area.y + 1,
        "header must sit below the top margin",
    );
}

/// Short terminals collapse the top margin to 0 so the row list still gets visible space.
/// The threshold matches the dispatch chrome's threshold (`area.height > 6`).
#[test]
fn layout_drops_top_margin_on_short_terminals() {
    let area = Rect::new(0, 0, 80, 6);
    let layout = compute_layout(area, false);
    assert_eq!(layout.top_margin.height, 0);
}

#[test]
fn layout_at_minimum_width_returns_valid_rect() {
    let area = Rect::new(0, 0, MIN_DASHBOARD_WIDTH, 30);
    let layout = compute_layout(area, false);
    // The list is inset by LIST_OUTER_HPAD on each side even at the minimum dashboard width (40 cols, 38 usable for content)
    assert_eq!(layout.list.width, MIN_DASHBOARD_WIDTH - LIST_OUTER_HPAD * 2);
}

// Boundary tests for zero-size and below-minimum areas

/// A zero-height area produces zero-height sub-rects (and doesn't panic on saturating subtraction).
#[test]
fn layout_height_zero_produces_zero_subrects() {
    let area = Rect::new(0, 0, 80, 0);
    let layout = compute_layout(area, true);
    assert_eq!(layout.header.height, 0);
    assert_eq!(layout.list.height, 0);
    assert_eq!(layout.dispatch.height, 0);
    assert_eq!(layout.footer.height, 0);
}

/// List-first: short heights cannot open a peek (remainder < peek min).
#[test]
fn allocate_peek_refuses_when_remainder_below_min() {
    let area = Rect::new(0, 0, 80, 24);
    let fixed = chrome_overhead(area);
    let alloc = allocate_peek(area.height, fixed, 20, PEEK_MIN_BOX_LIVE_TAIL);
    // Chrome takes 9 rows, leaving 15; the list floor of 12 leaves 3, below the peek min of 8, so no peek opens
    assert!(
        !alloc.show_peek,
        "h=24 should not fit list floor + peek min"
    );
}

/// Peek renders inside the dispatch rect, which grows when visible.
#[test]
fn layout_grows_dispatch_when_peek_visible() {
    let area = Rect::new(0, 0, 80, 40);
    let no_peek = compute_layout(area, false);
    let with_peek = compute_layout_with_dispatch(area, true, 12);
    assert!(
        with_peek.dispatch.height > no_peek.dispatch.height,
        "peek-visible must grow the dispatch rect, no_peek={} with_peek={}",
        no_peek.dispatch.height,
        with_peek.dispatch.height,
    );
    assert!(with_peek.list.height >= LIST_FLOOR_ROWS);
}

/// Larger desired content yields a taller peek box until the max fraction.
#[test]
fn peek_box_sizes_to_content_rows() {
    let area = Rect::new(0, 0, 80, 40);
    let small = compute_layout_with_dispatch(area, true, 6);
    let large = compute_layout_with_dispatch(area, true, 20);
    assert!(large.dispatch.height >= small.dispatch.height);
    assert!(large.list.height <= small.list.height);
    assert!(large.list.height >= LIST_FLOOR_ROWS);
    assert!(large.dispatch.height <= peek_max_box_rows(40));
}

/// Zero-width area returns valid zero-width rects.
#[test]
fn layout_width_zero_returns_zero_width_subrects() {
    let area = Rect::new(0, 0, 0, 30);
    let layout = compute_layout(area, false);
    assert_eq!(layout.list.width, 0);
    assert_eq!(layout.dispatch.width, 0);
}

/// A 39-wide area (below `MIN_DASHBOARD_WIDTH=40`) returns valid sub-rects; the renderer will fall back to narrow mode.
/// The list still receives its outer hpad inset.
#[test]
fn layout_width_below_min_returns_valid_subrects() {
    let area = Rect::new(0, 0, MIN_DASHBOARD_WIDTH - 1, 30);
    let layout = compute_layout(area, false);
    assert_eq!(
        layout.list.width,
        MIN_DASHBOARD_WIDTH - 1 - LIST_OUTER_HPAD * 2
    );
    assert!(layout.list.height > 0);
}

#[test]
fn max_peek_content_rows_zero_on_short_terminal() {
    assert_eq!(max_peek_content_rows(Rect::new(0, 0, 80, 8)), 0);
    assert_eq!(max_peek_content_rows(Rect::new(0, 0, 80, 1)), 0);
}

/// `h=29` is the shortest terminal that fits the 9 chrome rows, the 12-row list floor, and the 8-row peek minimum.
#[test]
fn allocate_peek_list_floor_and_max_fraction() {
    assert!(
        !allocate_peek(
            28,
            chrome_overhead(Rect::new(0, 0, 80, 28)),
            255,
            PEEK_MIN_BOX_LIVE_TAIL
        )
        .show_peek,
        "h=28 leaves only 7 rows past the list floor, below the peek minimum",
    );
    for h in [29u16, 32, 40, 60, 80] {
        let area = Rect::new(0, 0, 80, h);
        let fixed = chrome_overhead(area);
        let alloc = allocate_peek(h, fixed, 255, PEEK_MIN_BOX_LIVE_TAIL);
        assert!(alloc.show_peek, "h={h} should open peek");
        assert!(
            alloc.peek_box_h <= peek_max_box_rows(h),
            "h={h} peek {} > max {}",
            alloc.peek_box_h,
            peek_max_box_rows(h)
        );
        assert!(alloc.peek_box_h >= PEEK_MIN_BOX_LIVE_TAIL);
        let layout = compute_layout_with_peek_box(area, alloc.peek_box_h);
        assert!(
            layout.list.height >= LIST_FLOOR_ROWS,
            "h={h} list {} < floor",
            layout.list.height
        );
        assert_eq!(layout.dispatch.height, alloc.peek_box_h);
    }
}

#[test]
fn allocate_peek_respects_three_eighths_cap() {
    assert_eq!(peek_max_box_rows(40), 15); // floor(40*3/8)
    assert_eq!(peek_max_box_rows(60), 22);
    assert_eq!(peek_max_box_rows(8), 3);
}

#[test]
fn reply_growth_steals_from_list_down_to_floor_then_body() {
    let area = Rect::new(0, 0, 80, 40);
    let fixed = chrome_overhead(area);
    let one = allocate_peek(40, fixed, 6, PEEK_MIN_BOX_LIVE_TAIL);
    let multi = allocate_peek(40, fixed, 14, PEEK_MIN_BOX_LIVE_TAIL);
    assert!(one.show_peek && multi.show_peek);
    assert!(multi.peek_box_h >= one.peek_box_h);
    let layout_multi = compute_layout_with_peek_box(area, multi.peek_box_h);
    assert!(layout_multi.list.height >= LIST_FLOOR_ROWS);
    assert!(multi.peek_box_h <= peek_max_box_rows(40));
}

#[test]
fn layout_header_aligns_with_list_and_dispatch() {
    let area = Rect::new(0, 0, 80, 30);
    let layout = compute_layout(area, false);
    assert_eq!(layout.header.x, layout.list.x);
    assert_eq!(layout.header.x, layout.dispatch.x);
    assert_eq!(layout.header.width, layout.list.width);
    assert_eq!(layout.header.width, layout.dispatch.width);
}

#[test]
fn peek_live_tail_desired_empty_uses_one_body_row() {
    let d = peek_live_tail_desired_content(20, 1, 0, false);
    assert_eq!(d.live_tail, 1);
    assert!(d.blank_row, "empty/hint body still budgets blank when room");
    assert_eq!(d.content_rows, 1 + 1 + 1 + 1); // status+reply+blank+body
}

#[test]
fn peek_live_tail_desired_tight_pin_skips_blank() {
    // fixed is status + reply3 + pin = 5; max_content is fixed + 1, so body 1 and no blank
    let d = peek_live_tail_desired_content(6, 3, 1, true);
    assert!(!d.blank_row);
    assert_eq!(d.live_tail, 1);
    assert_eq!(d.content_rows, 1 + 3 + 1 + 1); // status+reply+pin+body
}

#[test]
fn peek_live_tail_desired_short_body_budgets_blank_and_pin() {
    let d = peek_live_tail_desired_content(40, 1, 2, false);
    assert_eq!(d.live_tail, 2);
    assert!(d.blank_row);
    assert_eq!(d.content_rows, 1 + 1 + 1 + 2);

    let with_pin = peek_live_tail_desired_content(40, 1, 2, true);
    assert_eq!(with_pin.live_tail, 2);
    assert!(with_pin.blank_row);
    assert_eq!(with_pin.content_rows, 1 + 1 + 1 + 1 + 2); // +pin
    assert!(with_pin.content_rows > d.content_rows);
}

#[test]
fn peek_live_tail_desired_long_body_hits_live_tail_cap() {
    let d = peek_live_tail_desired_content(80, 1, 200, false);
    assert_eq!(d.live_tail, MAX_LIVE_TAIL_ROWS);
    assert!(d.blank_row);
    assert_eq!(d.content_rows, 1 + 1 + 1 + MAX_LIVE_TAIL_ROWS);
}

#[test]
fn peek_live_tail_desired_pin_fits_in_measured_body_budget() {
    // A body that fits without the pin must not force an ellipsis solely due to the pin
    // The desired size grows by the pin row, so the paint body_budget still covers the body
    let body = 4u16;
    let d = peek_live_tail_desired_content(40, 1, body, true);
    assert_eq!(d.live_tail, body);
    assert_eq!(
        d.content_rows,
        1 + 1 + 1 + 1 + body,
        "status+reply+pin+blank+body"
    );
}

#[test]
fn peek_live_tail_desired_never_exceeds_max_content() {
    for max_content in 0..=40u16 {
        for reply in 1..=6u16 {
            for body in [0u16, 1, 3, 10, 50, 200] {
                for pin in [false, true] {
                    let d = peek_live_tail_desired_content(max_content, reply, body, pin);
                    assert!(
                        d.content_rows <= max_content,
                        "content_rows={} > max={max_content} reply={reply} body={body} pin={pin}",
                        d.content_rows
                    );
                    assert!(d.live_tail <= MAX_LIVE_TAIL_ROWS);
                }
            }
        }
    }
}
