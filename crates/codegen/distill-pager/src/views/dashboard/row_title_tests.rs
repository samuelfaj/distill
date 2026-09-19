use pretty_assertions::assert_eq;

use super::*;
use crate::views::dashboard::state::RowState;
use crate::views::dashboard::test_support::{buf_to_text, header_test_row};
use ratatui::style::Modifier;

fn fit_chips(badges: &[RowBadge], budget: usize, available: usize) -> String {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, "Session");
    row.badges = badges.to_vec();
    RowTitle {
        row: &row,
        theme: &theme,
        bg: theme.bg_base,
    }
    .fit_chips(budget, available)
    .to_string()
}

fn buf_cell(buf: &Buffer, x: u16, y: u16) -> &ratatui::buffer::Cell {
    buf.cell((x, y))
        .unwrap_or_else(|| panic!("missing cell ({x},{y})"))
}

#[test]
fn wide_chips_collapse_before_title_or_subtitle_is_truncated() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, "Session");
    row.subtitle = Some("repo".to_owned());
    row.badges = vec![
        RowBadge::Subagents(2),
        RowBadge::Tasks(3),
        RowBadge::Watchers(4),
        RowBadge::Workflows(1),
    ];
    let text = "Session · repo";
    for (chip_budget, chips) in [
        (80, "Subagents 2 · Tasks 3 · Watchers 4 · Workflows 1"),
        (37, "Subs 2 · Tasks 3 · Watch 4 · Flow 1"),
        (7, "10 bg"),
        (4, "bg"),
    ] {
        let width = (text.width() + 1 + chip_budget) as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 10, 1));
        buf.set_string(width + 2, 0, "   2 min", theme.dim());
        RowTitle {
            row: &row,
            theme: &theme,
            bg: theme.bg_base,
        }
        .render_wide(&mut buf, Rect::new(0, 0, width, 1));
        let expected = format!(
            "{text}{}{chips}     2 min\n",
            " ".repeat(usize::from(width) - text.width() - chips.width())
        );
        assert_eq!(expected, buf_to_text(&buf), "chip budget {chip_budget}");
    }
}

#[test]
fn narrow_chips_collapse_before_title_is_truncated() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, "Session");
    row.subtitle = Some("not rendered in narrow mode".repeat(10));
    row.badges = vec![RowBadge::Subagents(1), RowBadge::Workflows(1)];
    for chips in ["Subagents 1 · Workflows 1", "Sub 1 · Flow 1", "2 bg", "bg"] {
        let expected = format!("Session {chips}\n");
        let width = expected.trim_end().width() as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        RowTitle {
            row: &row,
            theme: &theme,
            bg: theme.bg_base,
        }
        .render_narrow(&mut buf, Rect::new(0, 0, width, 1));
        assert_eq!(expected, buf_to_text(&buf), "width {width}");
    }
}

#[test]
fn left_text_truncates_only_after_smallest_chip_remains() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, &"1234567890".repeat(10));
    for (badges, expected) in [
        (
            vec![RowBadge::Subagents(1), RowBadge::Workflows(1)],
            "12345678901… bg\n",
        ),
        (vec![RowBadge::Watchers(1)], "12345678901… bg\n"),
    ] {
        row.badges = badges;
        let area = Rect::new(0, 0, 15, 1);
        let mut wide = Buffer::empty(area);
        let mut narrow = Buffer::empty(area);
        let title = RowTitle {
            row: &row,
            theme: &theme,
            bg: theme.bg_base,
        };
        title.render_wide(&mut wide, area);
        title.render_narrow(&mut narrow, area);
        assert_eq!(expected, buf_to_text(&wide));
        assert_eq!(expected, buf_to_text(&narrow));
    }
}

#[test]
fn chip_budget_uses_display_width_for_unicode_left_text() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, "中e\u{301}☁\u{fe0f}");
    row.subtitle = Some("界".to_owned());
    row.badges = vec![RowBadge::Watchers(2)];
    let subtitle = " · 界";
    let chip = "Watchers 2";
    let width = (row.label.width() + subtitle.width() + 1 + chip.width()) as u16;
    let area = Rect::new(0, 0, width, 1);
    let mut actual = Buffer::empty(area);
    RowTitle {
        row: &row,
        theme: &theme,
        bg: theme.bg_base,
    }
    .render_wide(&mut actual, area);
    let mut expected = Buffer::empty(area);
    expected.set_string(
        0,
        0,
        &row.label,
        Style::default().bg(theme.bg_base).fg(theme.text_primary),
    );
    let x = row.label.width() as u16;
    expected.set_string(x, 0, subtitle, theme.dim().bg(theme.bg_base));
    let x = x + subtitle.width() as u16;
    expected.set_string(
        x + 1,
        0,
        "Watchers",
        Style::default()
            .fg(theme.gray_bright)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );
    expected.set_string(
        x + 9,
        0,
        " 2",
        Style::default().fg(theme.gray).bg(theme.bg_base),
    );
    assert_eq!(expected, actual);
}

#[test]
fn chip_forms_use_independent_name_count_separator_and_bg_styles() {
    for theme in [Theme::groknight(), Theme::grokday()] {
        let mut row = header_test_row(1, RowState::Working, "Session");
        row.badges = vec![
            RowBadge::Subagents(3),
            RowBadge::Tasks(4),
            RowBadge::Watchers(3),
            RowBadge::Workflows(2),
        ];
        for (chip, name_ranges) in [
            (
                "Subagents 3 · Tasks 4 · Watchers 3 · Workflows 2",
                vec![0..9, 14..19, 24..32, 37..46],
            ),
            (
                "Subs 3 · Tasks 4 · Watch 3 · Flows 2",
                vec![0..4, 9..14, 19..24, 29..34],
            ),
            ("12 bg", vec![]),
            ("bg", vec![]),
        ] {
            for bg in [theme.bg_base, theme.bg_hover, theme.bg_highlight] {
                for paint in [RowTitle::render_wide, RowTitle::render_narrow] {
                    let width = (row.label.width() + 1 + chip.width()) as u16;
                    let area = Rect::new(2, 1, width, 1);
                    let mut buf = Buffer::empty(Rect::new(0, 0, area.right() + 2, 3));
                    buf.set_style(area, Style::default().add_modifier(Modifier::BOLD));
                    paint(
                        &RowTitle {
                            row: &row,
                            theme: &theme,
                            bg,
                        },
                        &mut buf,
                        area,
                    );
                    let chip_x = area.right() - chip.width() as u16;
                    for (offset, symbol) in chip.chars().enumerate() {
                        let cell = buf_cell(&buf, chip_x + offset as u16, area.y);
                        let is_name = name_ranges.iter().any(|range| range.contains(&offset));
                        assert_eq!(symbol.to_string(), cell.symbol(), "{chip}: column {offset}");
                        assert_eq!(
                            if is_name {
                                theme.gray_bright
                            } else {
                                theme.gray
                            },
                            cell.fg,
                            "{chip}: column {offset} foreground",
                        );
                        assert_eq!(bg, cell.bg, "{chip}: column {offset} background");
                        assert_eq!(
                            if is_name {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            },
                            cell.modifier,
                            "{chip}: column {offset} modifiers",
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn failed_badge_counts_toward_left_text_width() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Failed, "Session");
    row.badges = vec![RowBadge::Failed, RowBadge::Watchers(2)];
    let expected = "Session · failed Watchers 2\n";
    let area = Rect::new(0, 0, expected.trim_end().width() as u16, 1);
    let mut buf = Buffer::empty(area);
    RowTitle {
        row: &row,
        theme: &theme,
        bg: theme.bg_base,
    }
    .render_wide(&mut buf, area);
    assert_eq!(expected, buf_to_text(&buf));
    assert_eq!(theme.accent_error, buf_cell(&buf, 11, 0).fg);
}

#[test]
fn rows_without_live_chips_keep_the_full_text_budget() {
    let theme = Theme::groknight();
    let row = header_test_row(1, RowState::Idle, "Exact fit");
    let area = Rect::new(0, 0, row.label.width() as u16, 1);
    for paint in [RowTitle::render_wide, RowTitle::render_narrow] {
        let mut buf = Buffer::empty(area);
        paint(
            &RowTitle {
                row: &row,
                theme: &theme,
                bg: theme.bg_base,
            },
            &mut buf,
            area,
        );
        assert_eq!("Exact fit\n", buf_to_text(&buf));
    }
}

#[test]
fn title_chips_full_and_short_labels_use_correct_counts() {
    let badges = [
        RowBadge::Subagents(1),
        RowBadge::Tasks(2),
        RowBadge::Watchers(3),
        RowBadge::Workflows(4),
    ];
    assert_eq!(
        "Subagents 1 · Tasks 2 · Watchers 3 · Workflows 4",
        fit_chips(&badges, 80, 80)
    );
    assert_eq!(
        "Sub 1 · Tasks 2 · Watch 3 · Flows 4",
        fit_chips(&badges, 40, 40)
    );
    assert_eq!("Tasks 1", fit_chips(&[RowBadge::Tasks(1)], 7, 7));
    assert_eq!("Task 1", fit_chips(&[RowBadge::Tasks(1)], 6, 6));
    assert_eq!("Watchers 1", fit_chips(&[RowBadge::Watchers(1)], 11, 11));
    assert_eq!("Watch 1", fit_chips(&[RowBadge::Watchers(1)], 7, 7));
    assert_eq!("Workflows 1", fit_chips(&[RowBadge::Workflows(1)], 12, 12));
    assert_eq!("Flow 1", fit_chips(&[RowBadge::Workflows(1)], 6, 6));
    assert_eq!(
        "",
        fit_chips(&[RowBadge::Tasks(0), RowBadge::Worktree], 80, 80)
    );
}

#[test]
fn title_chips_collapse_to_merged_count_after_short_labels() {
    let badges = [
        RowBadge::Subagents(3),
        RowBadge::Tasks(4),
        RowBadge::Watchers(3),
    ];
    let full = "Subagents 3 · Tasks 4 · Watchers 3";
    let short = "Subs 3 · Tasks 4 · Watch 3";
    assert_eq!(full, fit_chips(&badges, full.width(), full.width()));
    assert_eq!(short, fit_chips(&badges, short.width(), short.width()));
    assert_eq!(
        "10 bg",
        fit_chips(&badges, short.width() - 1, short.width() - 1)
    );
    assert_eq!("bg", fit_chips(&badges, 4, 4));
    assert_eq!("", fit_chips(&badges, 1, 1));
}

#[test]
fn sole_kind_collapses_to_bg_after_short_label() {
    let badges = [RowBadge::Watchers(3)];
    assert_eq!("Watchers 3", fit_chips(&badges, 10, 10));
    assert_eq!("Watch 3", fit_chips(&badges, 7, 7));
    assert_eq!("3 bg", fit_chips(&badges, 6, 6));
    assert_eq!("bg", fit_chips(&badges, 2, 2));
    assert_eq!("", fit_chips(&badges, 1, 1));
}

#[test]
fn long_unicode_title_reserves_chips_and_age() {
    let mut row = header_test_row(1, RowState::Working, &"中e\u{301}👩🏽\u{200d}💻".repeat(50));
    row.subtitle = Some("long branch subtitle".repeat(20));
    row.badges = vec![RowBadge::Tasks(2), RowBadge::Watchers(1)];
    let theme = Theme::groknight();
    for width in 8..110 {
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 10, 1));
        buf.set_string(width + 2, 0, "   2 min", Style::default());
        RowTitle {
            row: &row,
            theme: &theme,
            bg: theme.bg_highlight,
        }
        .render_wide(&mut buf, Rect::new(0, 0, width, 1));
        let text = buf_to_text(&buf);
        let chip = "bg";
        assert!(text.contains(chip), "width {width}: {text}");
        assert!(text.ends_with("   2 min\n"), "width {width}: {text}");
        assert_eq!(" ", buf_cell(&buf, width, 0).symbol());
        assert_eq!(" ", buf_cell(&buf, width + 1, 0).symbol());
        assert_eq!(
            theme.gray,
            buf_cell(&buf, width - chip.width() as u16, 0).fg
        );
        assert_eq!(theme.bg_highlight, buf_cell(&buf, width - 1, 0).bg);
    }
}

#[test]
fn narrow_title_keeps_complete_chip_without_changing_single_line_layout() {
    let mut row = header_test_row(1, RowState::Working, &"長いタイトル".repeat(30));
    row.badges = vec![RowBadge::Watchers(1)];
    row.secondary_line = Some("must not add a second line".to_owned());
    let theme = Theme::groknight();
    for width in 0..50 {
        let mut buf = Buffer::empty(Rect::new(0, 0, 50, 2));
        RowTitle {
            row: &row,
            theme: &theme,
            bg: theme.bg_hover,
        }
        .render_narrow(&mut buf, Rect::new(0, 0, width, 1));
        let text = buf_to_text(&buf);
        if width >= 2 {
            assert!(
                text.contains("Watchers 1")
                    || text.contains("Watch 1")
                    || text.contains("1 bg")
                    || text.contains("bg"),
                "width {width}: {text}"
            );
        }
        assert!((0..50).all(|x| buf_cell(&buf, x, 1).symbol() == " "));
        assert!((width..50).all(|x| buf_cell(&buf, x, 0).symbol() == " "));
    }
}

fn assert_emoji_presentation_bounds(
    label: &str,
    subtitle: &str,
    paint: impl Fn(&RowTitle<'_>, &mut Buffer, Rect),
) {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Working, label);
    row.subtitle = Some(subtitle.to_owned());
    row.badges = vec![RowBadge::Tasks(1)];
    for width in [24, 48] {
        let area = Rect::new(2, 1, width, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, area.right() + 10, 3));
        buf.set_string(area.right() + 2, area.y, "   2 min", theme.dim());
        let outside = buf.clone();
        paint(
            &RowTitle {
                row: &row,
                theme: &theme,
                bg: theme.bg_highlight,
            },
            &mut buf,
            area,
        );
        let chip = "bg";
        let chip_x = area.right() - chip.width() as u16;
        let painted = (chip_x..area.right())
            .map(|x| buf_cell(&buf, x, area.y).symbol())
            .collect::<String>();
        assert_eq!(chip, painted, "width {width}: {}", buf_to_text(&buf));
        assert_eq!(" ", buf_cell(&buf, chip_x - 1, area.y).symbol());
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if !area.contains((x, y).into()) {
                    assert_eq!(
                        outside.cell((x, y)),
                        buf.cell((x, y)),
                        "outside title at ({x}, {y})"
                    );
                }
            }
        }
    }
}

#[test]
fn emoji_presentation_wide_title_cannot_overwrite_chips_or_age() {
    assert_emoji_presentation_bounds(&"☁\u{fe0f}".repeat(80), "subtitle", |title, buf, area| {
        title.render_wide(buf, area);
    });
}

#[test]
fn emoji_presentation_wide_subtitle_cannot_overwrite_chips_or_age() {
    assert_emoji_presentation_bounds("Title", &"☁\u{fe0f}".repeat(80), |title, buf, area| {
        title.render_wide(buf, area);
    });
}

#[test]
fn emoji_presentation_narrow_title_cannot_overwrite_chips_or_right_edge() {
    assert_emoji_presentation_bounds(
        &"☁\u{fe0f}".repeat(80),
        &"☁\u{fe0f}".repeat(80),
        |title, buf, area| {
            title.render_narrow(buf, area);
        },
    );
}

#[test]
fn fallback_and_failed_title_styles_stay_consistent() {
    let theme = Theme::groknight();
    let mut row = header_test_row(1, RowState::Failed, "New session #abcd");
    row.badges = vec![RowBadge::Failed];
    let mut buf = Buffer::empty(Rect::new(0, 0, 50, 1));
    RowTitle {
        row: &row,
        theme: &theme,
        bg: theme.bg_hover,
    }
    .render_wide(&mut buf, Rect::new(0, 0, 50, 1));
    assert!(buf_to_text(&buf).contains("New session #abcd · failed"));
    assert_eq!(theme.text_primary, buf_cell(&buf, 0, 0).fg);
    assert_eq!(theme.gray_dim, buf_cell(&buf, 12, 0).fg);
    assert_eq!(theme.accent_error, buf_cell(&buf, 21, 0).fg);
}
