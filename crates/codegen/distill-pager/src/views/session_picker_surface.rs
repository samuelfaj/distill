// Modified for Distill by Samuel Fajreldines, 2026.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::app_view::SessionPickerEntry;
use crate::theme::Theme;

/// Which surface a picker fetch was issued for.
/// Results route back to the requesting host's storage only; a live picker on another host never absorbs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerHost {
    /// Welcome-screen picker (`session_picker_*` fields on `AppView`).
    Welcome,
    /// `/resume` modal on the active agent (`ActiveModal::SessionPicker`).
    AgentModal,
    /// Dashboard picker (`AppView::dashboard_session_picker`).
    Dashboard,
}

/// Shared by dashboard picker paint and input so a copy change cannot update only one site.
pub(crate) const DASHBOARD_PICKER_TITLE: &str = "Open session";

/// State for one session-picker incarnation.
/// Host-agnostic: everything a picker accumulates between open and dismiss, nothing about how a host renders it or maps its keys.
#[derive(Debug)]
pub struct SessionPickerSurface {
    /// Incarnation identity; results apply only when it matches.
    pub generation: u64,
    pub state: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    pub entries: Option<Vec<crate::app::app_view::SessionPickerEntry>>,
    pub loading: bool,
    pub lanes: crate::views::session_picker::SessionPickerLanes,
    pub content_results: Option<Vec<distill_shell::extensions::session_search::SearchSessionHit>>,
    pub content_loading: bool,
    /// Per-surface counters; the dashboard host does not share the welcome picker's `session_picker_list_seq` / `session_picker_deep_search_seq`.
    pub list_seq: u64,
    pub deep_search_seq: u64,
    /// Invalidates in-flight card-detail reads when this surface's rows or filters change.
    pub detail_seq: u64,
    pub entries_query: Option<String>,
    pub source_filter: crate::views::session_picker::SourceFilter,
    pub pending_delete: Option<crate::views::session_picker::PendingDelete>,
}

impl SessionPickerSurface {
    #[must_use]
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            state: crate::views::picker::PickerState::default(),
            window: crate::views::modal_window::ModalWindowState::new(),
            entries: None,
            loading: false,
            lanes: Default::default(),
            content_results: None,
            content_loading: false,
            list_seq: 0,
            deep_search_seq: 0,
            detail_seq: 0,
            entries_query: None,
            source_filter: Default::default(),
            pending_delete: None,
        }
    }
}

pub(crate) enum SessionPickerRenderMode<'a> {
    Fullscreen,
    Modal {
        window: &'a mut crate::views::modal_window::ModalWindowState,
        title: &'a str,
    },
}

pub(crate) struct SessionPickerRenderCtx<'a> {
    pub(crate) state: &'a mut crate::views::picker::PickerState,
    pub(crate) sessions: Option<&'a [SessionPickerEntry]>,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) loading: bool,
    pub(crate) pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    pub(crate) shortcuts_area: Option<Rect>,
    pub(crate) content_results:
        Option<&'a [distill_shell::extensions::session_search::SearchSessionHit]>,
    pub(crate) content_loading: bool,
    pub(crate) entries_query: Option<&'a str>,
    pub(crate) tick: u64,
    pub(crate) grouped: bool,
    pub(crate) source_filter: crate::views::session_picker::SourceFilter,
    pub(crate) pending_delete: bool,
    pub(crate) chat_mode: bool,
}

pub(crate) fn render_session_picker(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    mode: SessionPickerRenderMode<'_>,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    match mode {
        SessionPickerRenderMode::Fullscreen => {
            crate::views::welcome::render_session_picker_body(area, buf, theme, ctx)
        }
        SessionPickerRenderMode::Modal { window, title } => {
            render_simple_session_picker_modal(area, buf, theme, window, title, ctx)
        }
    }
}

const SESSION_SEARCH_LABEL: &str = " search: ";

pub(crate) fn render_session_picker_search_bar(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &crate::views::picker::PickerState,
) {
    crate::views::picker::render_picker_search_bar_with_label(
        buf,
        area.x,
        area.y,
        area.width,
        theme,
        SESSION_SEARCH_LABEL,
        state,
        state.search_active,
        true,
        Some(theme.bg_base),
    );
    let label_w = u16::try_from(SESSION_SEARCH_LABEL.len()).unwrap_or(0);
    if state.search_active {
        let width = label_w.min(area.width);
        if width > 0 {
            buf.set_style(
                Rect::new(area.x, area.y, width, 1),
                ratatui::style::Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            );
        }
    } else if !state.query().is_empty() {
        let start = area.x.saturating_add(label_w);
        let width = area.x.saturating_add(area.width).saturating_sub(start);
        if width > 0 {
            buf.set_style(Rect::new(start, area.y, width, 1), theme.dim());
        }
    }
}

fn empty_hit_areas() -> crate::views::picker::PickerHitAreas {
    crate::views::picker::PickerHitAreas {
        close_button: Rect::default(),
        search_bar: Rect::default(),
        item_rects: vec![],
        entry_indices: vec![],
        tab_rects: vec![],
        filter_rect: None,
    }
}

fn render_simple_session_picker_modal(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    window: &mut crate::views::modal_window::ModalWindowState,
    title: &str,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    use crate::views::modal_window::{ModalSizing, ModalWindowConfig, Shortcut};
    use crate::views::picker::{self, PickerField};

    let shortcuts = vec![
        Shortcut {
            label: "\u{2191}\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "/ search",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ];
    let modal_config = ModalWindowConfig {
        title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            width_pct: 0.65,
            max_width: 120,
            min_width: 48,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        },
        fold_info: None,
    };
    let Some(modal) =
        crate::views::modal_window::render_modal_window(buf, area, window, &modal_config, theme)
    else {
        ctx.state.hit_areas = None;
        return empty_hit_areas();
    };

    let content = modal.content;
    render_session_picker_search_bar(
        buf,
        Rect::new(content.x, content.y, content.width, 1),
        theme,
        ctx.state,
    );
    ctx.state.filter_area = None;
    let separator_y = content.y + 1;
    if separator_y < content.y + content.height {
        picker::render_divider(
            buf,
            modal.inner_x,
            separator_y,
            modal.inner_width,
            theme,
            Some(theme.bg_base),
        );
    }

    let query =
        crate::views::session_picker::effective_filter_query(ctx.state.query(), ctx.entries_query);
    let sessions = ctx.sessions.unwrap_or(&[]);
    let filtered =
        crate::app::app_view::filter_session_entries(ctx.sessions, query, ctx.source_filter);
    let built = crate::views::session_picker::build_session_entry_data(
        sessions,
        &filtered,
        ctx.state,
        content.width,
    );
    let fields: Vec<Vec<PickerField<'_>>> = built
        .iter()
        .map(|entry| {
            entry
                .field_data
                .iter()
                .map(|(label, value)| PickerField { label, value })
                .collect()
        })
        .collect();
    let current_repo = crate::views::session_picker::repo_name_from_cwd(&ctx.cwd.to_string_lossy());
    let (entries, non_selectable) = crate::views::session_picker::build_grouped_picker_entries(
        sessions,
        &filtered,
        &built,
        &fields,
        ctx.state,
        Some(current_repo.as_str()),
    );
    let entries_area = Rect {
        x: content.x,
        y: separator_y + 1,
        width: content.width,
        height: content
            .height
            .saturating_sub(separator_y.saturating_add(1).saturating_sub(content.y)),
    };
    let content_hit = picker::render_picker_content_with_scrollbar_x(
        buf,
        entries_area,
        theme,
        ctx.state,
        &entries,
        &non_selectable,
        &[],
        Some(theme.bg_base),
        ctx.loading,
        ctx.tick,
        modal.inner_x + modal.inner_width - 1,
    );
    crate::views::picker::PickerHitAreas {
        close_button: Rect::default(),
        search_bar: Rect::new(content.x, content.y, content.width, 1),
        item_rects: content_hit.item_rects,
        entry_indices: content_hit.entry_indices,
        tab_rects: vec![],
        filter_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    use super::render_session_picker_search_bar;
    use crate::theme::Theme;

    fn paint_inactive_query(theme: &Theme) -> Buffer {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut state = crate::views::picker::PickerState::default();
        state.search_active = false;
        state.set_query("alpha");
        render_session_picker_search_bar(&mut buf, area, theme, &state);
        buf
    }

    fn row_text(buf: &Buffer) -> String {
        (0..buf.area.width).fold(String::new(), |mut text, x| {
            if let Some(cell) = buf.cell((x, 0)) {
                text.push_str(cell.symbol());
            }
            text
        })
    }

    fn assert_inactive_query_is_dim(theme: &Theme) {
        let buf = paint_inactive_query(theme);
        let text = row_text(&buf);
        assert!(
            text.contains(" search:"),
            "idle query must keep the search label, got {text:?}"
        );
        assert!(
            !text.contains(">search:"),
            "idle query must not use the editing marker, got {text:?}"
        );
        assert!(
            text.contains("alpha"),
            "idle query must keep the typed text, got {text:?}"
        );

        let label_w = u16::try_from(" search: ".len()).unwrap_or(0);
        let mut saw_query = false;
        for x in label_w..buf.area.width {
            let Some(cell) = buf.cell((x, 0)) else {
                continue;
            };
            if cell.symbol().trim().is_empty() {
                continue;
            }
            saw_query = true;
            let dim = theme.dim();
            if theme.is_bandless() {
                assert!(
                    cell.modifier.contains(Modifier::DIM),
                    "terminal-native idle query must use DIM, got {cell:?}"
                );
            } else if let Some(fg) = dim.fg {
                assert_eq!(cell.fg, fg, "idle query must use theme.dim(), got {cell:?}");
            }
        }
        assert!(saw_query, "query glyphs missing from {text:?}");

        let caret = (0..buf.area.width).any(|x| {
            buf.cell((x, 0)).is_some_and(|cell| {
                if theme.is_bandless() {
                    cell.modifier.contains(Modifier::REVERSED)
                } else {
                    cell.bg == theme.text_primary
                }
            })
        });
        assert!(!caret, "idle query must not paint a caret, got {text:?}");
    }

    #[test]
    fn inactive_nonempty_query_is_dim_without_a_caret() {
        assert_inactive_query_is_dim(&Theme::groknight());
        assert_inactive_query_is_dim(&Theme::terminal());
    }

    #[test]
    fn focused_search_label_uses_the_title_color() {
        let theme = Theme::groknight();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut state = crate::views::picker::PickerState::default();
        state.search_active = true;
        render_session_picker_search_bar(&mut buf, area, &theme, &state);
        let text = row_text(&buf);
        assert!(text.contains(" search:"), "{text:?}");
        assert!(!text.contains('>'), "{text:?}");
        let labeled = (0..buf.area.width).any(|x| {
            buf.cell((x, 0)).is_some_and(|cell| {
                cell.symbol() == "s"
                    && cell.fg == theme.text_primary
                    && cell.modifier.contains(Modifier::BOLD)
            })
        });
        assert!(labeled, "focused label must match the modal title color");
    }
}
