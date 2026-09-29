//! The editable model tier form used by the welcome menu and `/tiers`.

use crate::acp::ModelState;
use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierField {
    Main,
    Worker,
    Utility,
}

impl TierField {
    fn next(self) -> Self {
        match self {
            Self::Main => Self::Worker,
            Self::Worker => Self::Utility,
            Self::Utility => Self::Main,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Main => Self::Utility,
            Self::Worker => Self::Main,
            Self::Utility => Self::Worker,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TierEditorOutcome {
    Submitted {
        main: String,
        worker: String,
        utility: String,
    },
    Cancelled,
    Changed,
    Unchanged,
}

#[derive(Debug)]
pub struct TierEditorState {
    pub main: LineEditor,
    pub worker: LineEditor,
    pub utility: LineEditor,
    pub focus: TierField,
}

impl TierEditorState {
    pub fn from_models(models: &ModelState) -> Self {
        let mut state = Self {
            main: LineEditor::default(),
            worker: LineEditor::default(),
            utility: LineEditor::default(),
            focus: TierField::Main,
        };
        // Each tier shows its effort after the model, as the fields accept it.
        let main_effort = if models.effort_auto {
            None
        } else {
            models.reasoning_effort
        };
        if let Some(main) = models.current_model_id_str() {
            state.main.set_text(&format!(
                "{main} {}",
                crate::acp::model_state::effort_setting_label(main_effort)
            ));
        }
        if let Some(worker) = models.worker_model.as_ref() {
            state.worker.set_text(&format!(
                "{} {}",
                worker.0,
                crate::acp::model_state::effort_setting_label(models.worker_effort)
            ));
        }
        state.utility.set_text(
            distill_shell::jev::local_config_cached()
                .model
                .as_deref()
                .unwrap_or_default(),
        );
        state
    }

    pub fn active_editor_mut(&mut self) -> &mut LineEditor {
        match self.focus {
            TierField::Main => &mut self.main,
            TierField::Worker => &mut self.worker,
            TierField::Utility => &mut self.utility,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> TierEditorOutcome {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return TierEditorOutcome::Unchanged;
        }
        if key.code == KeyCode::Esc
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd' | 'q'))
        {
            return TierEditorOutcome::Cancelled;
        }
        if key.code == KeyCode::Tab {
            self.focus = if key.modifiers.contains(KeyModifiers::SHIFT) {
                self.focus.previous()
            } else {
                self.focus.next()
            };
            return TierEditorOutcome::Changed;
        }
        match key.code {
            KeyCode::Up => {
                self.focus = self.focus.previous();
                return TierEditorOutcome::Changed;
            }
            KeyCode::Down => {
                self.focus = self.focus.next();
                return TierEditorOutcome::Changed;
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                return TierEditorOutcome::Submitted {
                    main: self.main.text().trim().to_owned(),
                    worker: self.worker.text().trim().to_owned(),
                    utility: self.utility.text().trim().to_owned(),
                };
            }
            _ => {}
        }
        match self
            .active_editor_mut()
            .handle_key_with_insert_policy(key, |character| !character.is_control())
        {
            LineEditOutcome::Unhandled => TierEditorOutcome::Unchanged,
            LineEditOutcome::HandledNoChange
            | LineEditOutcome::CursorChanged
            | LineEditOutcome::TextChanged => TierEditorOutcome::Changed,
        }
    }

    pub fn insert_paste(&mut self, text: &str) -> TierEditorOutcome {
        match self.active_editor_mut().insert_paste(text) {
            LineEditOutcome::Unhandled => TierEditorOutcome::Unchanged,
            LineEditOutcome::HandledNoChange
            | LineEditOutcome::CursorChanged
            | LineEditOutcome::TextChanged => TierEditorOutcome::Changed,
        }
    }
}

pub fn render_tier_editor_overlay(
    buf: &mut Buffer,
    area: Rect,
    window: &mut super::modal_window::ModalWindowState,
    state: &mut TierEditorState,
    compact: bool,
    theme: &Theme,
) {
    use super::modal_window::{self as mw, ModalSizing, ModalWindowConfig, Shortcut};

    let shortcuts = [
        Shortcut {
            label: "Tab next field",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter save",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc cancel",
            clickable: false,
            id: 0,
        },
    ];
    let config = ModalWindowConfig {
        title: "Model tiers",
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::large().with_compact(compact),
        fold_info: None,
    };
    let Some(content) = mw::render_modal_window(buf, area, window, &config, theme) else {
        return;
    };

    let mut y = content.content.y;
    render_text_line(
        buf,
        content.content,
        y,
        "Choose the main, worker, and utility models.",
        Style::default().fg(theme.text_primary),
    );
    y = y.saturating_add(1);
    render_text_line(
        buf,
        content.content,
        y,
        "The main model plans, delegates and reviews; the worker runs the delegated work.",
        Style::default().fg(theme.gray_bright),
    );
    y = y.saturating_add(2);

    render_field(
        buf,
        content.content,
        y,
        content.content.width,
        "Main model",
        "Required. Owns every session. A configured model id, then an effort level or auto.",
        &mut state.main,
        state.focus == TierField::Main,
        theme,
    );
    y = y.saturating_add(3);
    render_field(
        buf,
        content.content,
        y,
        content.content.width,
        "Worker model",
        "Optional. Runs the delegated work. A model id, then an effort level or auto; empty lets the main model do it all.",
        &mut state.worker,
        state.focus == TierField::Worker,
        theme,
    );
    y = y.saturating_add(3);
    render_field(
        buf,
        content.content,
        y,
        content.content.width,
        "Utility model",
        "A lower-cost model for short or fallback work. Use an OpenRouter id or a comma-separated chain.",
        &mut state.utility,
        state.focus == TierField::Utility,
        theme,
    );
}

fn render_field(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    width: u16,
    label: &str,
    description: &str,
    editor: &mut LineEditor,
    focused: bool,
    theme: &Theme,
) {
    if y >= area.y + area.height {
        return;
    }
    let label_style = Style::default()
        .fg(if focused {
            theme.fuzzy_accent
        } else {
            theme.text_primary
        })
        .add_modifier(Modifier::BOLD);
    Line::from(Span::styled(label, label_style)).render(Rect::new(area.x, y, width, 1), buf);
    if y + 1 < area.y + area.height {
        Line::from(Span::styled(
            description,
            Style::default().fg(theme.gray_bright),
        ))
        .render(Rect::new(area.x, y + 1, width, 1), buf);
    }
    if y + 2 >= area.y + area.height {
        return;
    }
    let prefix = "> ";
    let input_width = width.saturating_sub(prefix.len() as u16).max(1) as usize;
    let viewport = editor.viewport(input_width);
    let visible = editor
        .text()
        .get(viewport.visible_byte_range)
        .unwrap_or_default();
    let style = if focused {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_highlight)
    } else {
        Style::default().fg(theme.gray_bright)
    };
    for x in area.x..area.x + width {
        if let Some(cell) = buf.cell_mut((x, y + 2)) {
            cell.set_style(style);
            cell.set_char(' ');
        }
    }
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.fuzzy_accent)),
        Span::styled(visible, style),
    ])
    .render(Rect::new(area.x, y + 2, width, 1), buf);
    if focused {
        let cursor_x = area
            .x
            .saturating_add(prefix.len() as u16)
            .saturating_add(viewport.cursor_display_column as u16);
        if cursor_x < area.x + width
            && let Some(cell) = buf.cell_mut((cursor_x, y + 2))
        {
            cell.set_style(theme.block_cursor_over(theme.bg_highlight));
        }
    }
}

fn render_text_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    if y < area.y + area.height {
        Line::from(Span::styled(text, style)).render(Rect::new(area.x, y, area.width, 1), buf);
    }
}

#[cfg(test)]
mod tests {
    use super::{TierEditorOutcome, TierEditorState, TierField};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// The screen opens on what is saved: each tier's model followed by its
    /// effort, in the same `model effort` form the fields accept back.
    #[test]
    fn fields_open_with_each_tiers_model_and_effort() {
        use distill_shell::sampling::types::ReasoningEffort;
        let mut models = crate::acp::ModelState::default();
        let main = agent_client_protocol::ModelId::new("chatgpt/gpt-6-sol");
        models.available.insert(
            main.clone(),
            agent_client_protocol::ModelInfo::new(main.clone(), "GPT-6-Sol"),
        );
        models.current = Some(main);
        models.effort_auto = true;
        models.worker_model = Some(agent_client_protocol::ModelId::new("chatgpt/gpt-6-luna"));
        models.worker_effort = Some(ReasoningEffort::Medium);
        let state = TierEditorState::from_models(&models);
        assert_eq!(state.main.text(), "chatgpt/gpt-6-sol auto");
        assert_eq!(state.worker.text(), "chatgpt/gpt-6-luna medium");

        models.effort_auto = false;
        models.reasoning_effort = Some(ReasoningEffort::High);
        models.worker_effort = None;
        let state = TierEditorState::from_models(&models);
        assert_eq!(state.main.text(), "chatgpt/gpt-6-sol high");
        assert_eq!(state.worker.text(), "chatgpt/gpt-6-luna auto");
    }

    #[test]
    fn fields_cycle_and_submit_trimmed_values() {
        let mut state = TierEditorState {
            main: Default::default(),
            worker: Default::default(),
            utility: Default::default(),
            focus: TierField::Main,
        };
        state.main.set_text("  main  ");
        state.worker.set_text("worker");
        state.utility.set_text("one/two,three/four");
        assert_eq!(
            state.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE)),
            TierEditorOutcome::Changed
        );
        assert_eq!(state.focus, TierField::Worker);
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            TierEditorOutcome::Submitted {
                main: "main".to_owned(),
                worker: "worker".to_owned(),
                utility: "one/two,three/four".to_owned(),
            }
        );
    }

    #[test]
    fn escape_cancels_without_submitting() {
        let mut state = TierEditorState {
            main: Default::default(),
            worker: Default::default(),
            utility: Default::default(),
            focus: TierField::Main,
        };
        assert_eq!(
            state.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
            TierEditorOutcome::Cancelled
        );
    }
}
