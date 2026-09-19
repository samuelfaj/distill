use super::task_status_line;
use crate::theme::Theme;
use crate::views::tasks_pane::TaskStatusCounts;
use ratatui::style::Modifier;
use ratatui::text::Line;

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn hidden_for_zero_counts() {
    let theme = Theme::groknight();
    assert!(task_status_line(TaskStatusCounts::default(), &theme, false).is_none());
}

#[test]
fn running_is_a_static_diamond_not_a_spinner() {
    let theme = Theme::groknight();
    let counts = TaskStatusCounts {
        running: 2,
        paused_workflows: 0,
    };
    let first = task_status_line(counts, &theme, false).expect("running line");

    assert_eq!(
        line_text(&first),
        format!("{} 2", crate::glyphs::diamond_filled())
    );
    assert_eq!(first.spans.len(), 1);
    let Some(span) = first.spans.first() else {
        panic!("expected a span: {first:?}");
    };
    assert_eq!(span.style.fg, Some(theme.accent_running));
    assert_eq!(span.style.bg, Some(theme.bg_base));
}

#[test]
fn paused_is_static_warning_styled_and_hover_bold() {
    let _theme = crate::theme::cache::pin_theme();
    let theme = Theme::current();
    let counts = TaskStatusCounts {
        running: 0,
        paused_workflows: 3,
    };
    let first = task_status_line(counts, &theme, false).expect("paused line");
    let hovered = task_status_line(counts, &theme, true).expect("paused line");

    assert_eq!(line_text(&first), "P 3");
    let Some(span) = first.spans.first() else {
        panic!("expected a span: {first:?}");
    };
    assert_eq!(span.style.fg, Some(theme.warning));
    assert_eq!(span.style.bg, Some(theme.bg_base));
    assert!(
        hovered
            .spans
            .first()
            .is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD))
    );
}

#[test]
fn mixed_uses_separate_styles_and_neither_animates() {
    let theme = Theme::groknight();
    let counts = TaskStatusCounts {
        running: 1,
        paused_workflows: 2,
    };
    let line = task_status_line(counts, &theme, false).expect("mixed line");

    let [running, paused] = line.spans.as_slice() else {
        panic!("expected two spans: {line:?}");
    };
    assert_eq!(running.style.fg, Some(theme.accent_running));
    assert_eq!(paused.style.fg, Some(theme.warning));
    assert_eq!(
        running.content,
        format!("{} 1", crate::glyphs::diamond_filled())
    );
    assert_eq!(paused.content, "  P 2");
}
