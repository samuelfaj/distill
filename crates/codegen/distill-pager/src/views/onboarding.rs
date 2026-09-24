//! Four-step first-run onboarding for the Distill TUI.
//!
//! The state is deliberately independent from authentication and model storage. The host
//! translates the small commands below into the existing login, `/model`, worker, persistence,
//! and browser actions, so closing this overlay never creates a second account or model path.

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use crate::app::actions::LoginProvider;
use crate::theme::Theme;

pub const STEP_COUNT: usize = 4;
pub const X_URL: &str = "https://x.com/samfajreldines/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Budget,
    Connect,
    Worker,
    Community,
}

impl OnboardingStep {
    fn index(self) -> usize {
        match self {
            Self::Budget => 0,
            Self::Connect => 1,
            Self::Worker => 2,
            Self::Community => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        match index.min(STEP_COUNT - 1) {
            0 => Self::Budget,
            1 => Self::Connect,
            2 => Self::Worker,
            _ => Self::Community,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Budget => "Make your AI budget go further",
            Self::Connect => "Connect your AI",
            Self::Worker => "Choose a worker model",
            Self::Community => "Stay in the loop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingCommand {
    Close,
    Back,
    Continue,
    LoginGrok,
    LoginProvider(LoginProvider),
    CancelGrokLogin,
    CancelProviderLogin(LoginProvider),
    SelectModel(usize),
    SelectWorker(usize),
    OpenX,
    Complete,
}

#[derive(Debug, Clone)]
pub struct OnboardingState {
    pub step: OnboardingStep,
    pub selected: usize,
    pub auth_pending: bool,
    pub auth_provider: Option<LoginProvider>,
    pub completion_pending: bool,
    setting_pending: Option<PendingSetting>,
    pub status: Option<String>,
    pub status_is_error: bool,
    pub x_requested: bool,
    pub pulse: u8,
    pub scroll: usize,
    follow_selection: bool,
    hit_rows: Vec<(Rect, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingSetting {
    PrimaryModel,
    WorkerModel,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self::new()
    }
}

impl OnboardingState {
    pub fn new() -> Self {
        Self {
            step: OnboardingStep::Budget,
            selected: 0,
            auth_pending: false,
            auth_provider: None,
            completion_pending: false,
            setting_pending: None,
            status: None,
            status_is_error: false,
            x_requested: false,
            pulse: 0,
            scroll: 0,
            follow_selection: true,
            hit_rows: Vec::new(),
        }
    }

    pub fn step_number(&self) -> usize {
        self.step.index() + 1
    }

    pub fn set_auth_started(&mut self, provider: Option<LoginProvider>) {
        self.auth_pending = true;
        self.auth_provider = provider;
        self.status = Some(match provider {
            Some(provider) => format!(
                "{} login is waiting for the browser. Esc cancels this onboarding login.",
                provider.name()
            ),
            None => "Grok login is waiting for the browser. Esc cancels this onboarding login."
                .to_owned(),
        });
        self.status_is_error = false;
    }

    pub fn is_grok_auth_pending(&self) -> bool {
        self.auth_pending && self.auth_provider.is_none()
    }

    pub fn set_auth_result(&mut self, success: bool, message: impl Into<String>) {
        self.set_step(OnboardingStep::Connect);
        self.auth_pending = false;
        self.auth_provider = None;
        self.status = Some(message.into());
        self.status_is_error = !success;
    }

    pub fn set_info(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_is_error = false;
    }

    pub fn set_auth_browser_fallback(&mut self, url: &str) {
        self.status = Some(format!(
            "The browser could not open automatically. Use this URL, then return here:\n{url}"
        ));
        self.status_is_error = true;
    }

    pub fn set_persistence_error(&mut self, error: impl Into<String>) {
        self.completion_pending = false;
        self.status = Some(format!(
            "Could not save onboarding completion: {}. Nothing was marked complete; retry with Enter.",
            error.into()
        ));
        self.status_is_error = true;
    }

    pub fn set_primary_model_pending(&mut self) {
        self.setting_pending = Some(PendingSetting::PrimaryModel);
        self.status = Some("Saving the secondary reasoning model…".to_owned());
        self.status_is_error = false;
    }

    pub fn set_worker_model_pending(&mut self) {
        self.setting_pending = Some(PendingSetting::WorkerModel);
        self.status = Some("Saving the worker model…".to_owned());
        self.status_is_error = false;
    }

    pub fn finish_setting_persistence(
        &mut self,
        key: &str,
        success: bool,
        message: impl Into<String>,
    ) {
        let expected = match key {
            "default_model" => PendingSetting::PrimaryModel,
            "tier_light" => PendingSetting::WorkerModel,
            _ => return,
        };
        if self.setting_pending != Some(expected) {
            return;
        }
        self.setting_pending = None;
        self.status = Some(message.into());
        self.status_is_error = !success;
    }

    pub fn set_browser_requested(&mut self) {
        self.x_requested = true;
        self.status = Some(
            "Browser opener requested. If it did not open, use the URL below; Finish remains available."
                .to_owned(),
        );
        self.status_is_error = false;
    }

    fn set_step(&mut self, step: OnboardingStep) {
        self.step = step;
        self.selected = 0;
        self.scroll = 0;
        self.follow_selection = true;
        self.status = None;
        self.status_is_error = false;
    }

    fn item_count(&self, model_count: usize, worker_count: usize) -> usize {
        match self.step {
            OnboardingStep::Budget => 1,
            OnboardingStep::Connect => 4 + model_count,
            OnboardingStep::Worker => worker_count + 2,
            OnboardingStep::Community => 2,
        }
    }

    fn activate(&mut self, model_count: usize, worker_count: usize) -> Option<OnboardingCommand> {
        match self.step {
            OnboardingStep::Budget => {
                self.set_step(OnboardingStep::Connect);
                Some(OnboardingCommand::Continue)
            }
            OnboardingStep::Connect => match self.selected {
                0 => {
                    self.set_auth_started(None);
                    Some(OnboardingCommand::LoginGrok)
                }
                1 => {
                    self.set_auth_started(Some(LoginProvider::ChatGpt));
                    Some(OnboardingCommand::LoginProvider(LoginProvider::ChatGpt))
                }
                2 => {
                    self.set_auth_started(Some(LoginProvider::OpenRouter));
                    Some(OnboardingCommand::LoginProvider(LoginProvider::OpenRouter))
                }
                selected if selected < 3 + model_count => {
                    Some(OnboardingCommand::SelectModel(selected - 3))
                }
                _ => {
                    self.set_step(OnboardingStep::Worker);
                    Some(OnboardingCommand::Continue)
                }
            },
            OnboardingStep::Worker => {
                if self.selected < worker_count {
                    Some(OnboardingCommand::SelectWorker(self.selected))
                } else if self.selected == worker_count {
                    self.set_step(OnboardingStep::Community);
                    Some(OnboardingCommand::Continue)
                } else {
                    self.set_step(OnboardingStep::Community);
                    Some(OnboardingCommand::Continue)
                }
            }
            OnboardingStep::Community => {
                if self.selected == 0 {
                    self.set_browser_requested();
                    Some(OnboardingCommand::OpenX)
                } else {
                    Some(OnboardingCommand::Complete)
                }
            }
        }
    }

    pub fn handle_input(
        &mut self,
        ev: &Event,
        model_count: usize,
        worker_count: usize,
    ) -> Option<OnboardingCommand> {
        if self.completion_pending || self.setting_pending.is_some() {
            return None;
        }
        if let Event::Mouse(mouse) = ev {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll = self.scroll.saturating_sub(3);
                    self.follow_selection = false;
                    return None;
                }
                MouseEventKind::ScrollDown => {
                    self.scroll = self.scroll.saturating_add(3);
                    self.follow_selection = false;
                    return None;
                }
                _ => {}
            }
            if self.auth_pending {
                return None;
            }
            if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                return None;
            }
            let position = Position::new(mouse.column, mouse.row);
            if let Some((_, index)) = self
                .hit_rows
                .iter()
                .find(|(rect, _)| rect.contains(position))
            {
                self.selected = *index;
                self.follow_selection = true;
                return self.activate(model_count, worker_count);
            }
            return None;
        }

        let Event::Key(key) = ev else {
            return None;
        };
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            if self.auth_pending {
                return match self.auth_provider {
                    Some(provider) => Some(OnboardingCommand::CancelProviderLogin(provider)),
                    None => Some(OnboardingCommand::CancelGrokLogin),
                };
            }
            return Some(OnboardingCommand::Close);
        }
        if self.auth_pending {
            return None;
        }
        match key.code {
            KeyCode::Up => {
                let count = self.item_count(model_count, worker_count);
                self.selected = if self.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                self.follow_selection = true;
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                let count = self.item_count(model_count, worker_count).max(1);
                self.selected = (self.selected + 1) % count;
                self.follow_selection = true;
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
                self.follow_selection = false;
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5);
                self.follow_selection = false;
                None
            }
            KeyCode::Home => {
                self.selected = 0;
                self.scroll = 0;
                self.follow_selection = true;
                None
            }
            KeyCode::End => {
                self.selected = self.item_count(model_count, worker_count).saturating_sub(1);
                self.follow_selection = true;
                None
            }
            KeyCode::Left | KeyCode::Backspace => {
                if self.step.index() > 0 {
                    self.set_step(OnboardingStep::from_index(self.step.index() - 1));
                }
                None
            }
            KeyCode::Right | KeyCode::Enter => self.activate(model_count, worker_count),
            KeyCode::Char('s') => match self.step {
                OnboardingStep::Budget | OnboardingStep::Connect => {
                    self.set_step(OnboardingStep::from_index(self.step.index() + 1));
                    Some(OnboardingCommand::Continue)
                }
                OnboardingStep::Worker => {
                    self.set_step(OnboardingStep::Community);
                    Some(OnboardingCommand::Continue)
                }
                OnboardingStep::Community => Some(OnboardingCommand::Complete),
            },
            _ => None,
        }
    }
}

fn popup_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(2).min(84);
    let height = area.height.saturating_sub(2).min(26);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn row_line(
    index: usize,
    selected: usize,
    label: impl Into<String>,
    theme: &Theme,
) -> Line<'static> {
    let label = label.into();
    let marker = if index == selected { "› " } else { "  " };
    let style = if index == selected {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_highlight)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_secondary)
    };
    Line::from(Span::styled(format!("{marker}{label}"), style))
}

fn push_choice_row(
    lines: &mut Vec<Line<'static>>,
    row_lines: &mut Vec<(usize, usize)>,
    index: usize,
    selected: usize,
    label: impl Into<String>,
    theme: &Theme,
) {
    let line_index = lines.len();
    lines.push(row_line(index, selected, label, theme));
    row_lines.push((index, line_index));
}

pub fn render_onboarding(
    buf: &mut Buffer,
    area: Rect,
    state: &mut OnboardingState,
    compact: bool,
    models: &[(String, String)],
    workers: &[(String, String)],
    current_worker: Option<&str>,
    provider_auth: Option<crate::app::actions::ProviderAuthState>,
) {
    state.pulse = state.pulse.wrapping_add(1);
    state.hit_rows.clear();
    let popup = popup_area(area);
    if popup.width < 20 || popup.height < 7 {
        Clear.render(area, buf);
        Paragraph::new("Onboarding needs a slightly larger terminal. Resize, then press Enter.")
            .style(Style::default().fg(Theme::current().text_primary))
            .wrap(Wrap { trim: true })
            .render(area, buf);
        return;
    }

    let theme = Theme::current();
    Clear.render(popup, buf);
    let title = format!(
        "{}  ·  Step {} of {}",
        state.step.title(),
        state.step_number(),
        STEP_COUNT
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.gray_dim))
        .style(Style::default().bg(theme.bg_base));
    let inner = block.inner(popup);
    block.render(popup, buf);

    let mut lines = Vec::new();
    let mut row_lines = Vec::new();
    match state.step {
        OnboardingStep::Budget => {
            let mark = if state.pulse % 2 == 0 { "✦" } else { "·" };
            lines.push(Line::from(Span::styled(
                format!("{mark} One clear idea for using Distill economically:"),
                Style::default()
                    .fg(theme.accent_success)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from("OpenRouter can add model choices and prices. When a suitable less-expensive model handles a task, that may lower cost."));
            lines.push(Line::from("Distill can route different work to different models. Cost, total tokens, and tokens processed by an expensive model are separate measures."));
            lines.push(Line::from("OpenRouter alone does not reduce tokens. It changes available models and routing options."));
            lines.push(Line::from(""));
            push_choice_row(
                &mut lines,
                &mut row_lines,
                0,
                state.selected,
                "Continue",
                &theme,
            );
        }
        OnboardingStep::Connect => {
            lines.push(Line::from(
                "Reuse an existing account, then choose a reasoning model for planning and review.",
            ));
            lines.push(Line::from("Logins return here after success, error, or cancellation; account status updates here before you choose a model."));
            lines.push(Line::from(""));
            let auth = provider_auth.unwrap_or_default();
            push_choice_row(
                &mut lines,
                &mut row_lines,
                0,
                state.selected,
                format!(
                    "Grok account {}",
                    if auth.grok { "(connected)" } else { "(login)" }
                ),
                &theme,
            );
            push_choice_row(
                &mut lines,
                &mut row_lines,
                1,
                state.selected,
                format!(
                    "Codex (ChatGPT) {}",
                    if auth.chatgpt {
                        "(connected)"
                    } else {
                        "(login)"
                    }
                ),
                &theme,
            );
            push_choice_row(
                &mut lines,
                &mut row_lines,
                2,
                state.selected,
                format!(
                    "OpenRouter {}",
                    if auth.openrouter {
                        "(connected)"
                    } else {
                        "(login)"
                    }
                ),
                &theme,
            );
            for (index, (id, name)) in models.iter().enumerate() {
                push_choice_row(
                    &mut lines,
                    &mut row_lines,
                    index + 3,
                    state.selected,
                    format!("Reasoning model: {name} [{id}]"),
                    &theme,
                );
            }
            push_choice_row(
                &mut lines,
                &mut row_lines,
                3 + models.len(),
                state.selected,
                "Continue without changing the reasoning model",
                &theme,
            );
        }
        OnboardingStep::Worker => {
            lines.push(Line::from(
                "The worker owns the session; the reasoning model handles bounded planning and review.",
            ));
            lines.push(Line::from("Automatic routing requires effort set to auto. A worker must also share a compatible backend, connection, and credentials."));
            if let Some(current) = current_worker.filter(|value| !value.is_empty()) {
                lines.push(Line::from(format!(
                    "Current worker: {current} (Skip keeps it)"
                )));
            }
            if workers.is_empty() {
                lines.push(Line::from("No compatible worker is available in the current catalog. Skip keeps the existing configuration."));
            } else {
                for (index, (id, name)) in workers.iter().enumerate() {
                    push_choice_row(
                        &mut lines,
                        &mut row_lines,
                        index,
                        state.selected,
                        format!("{name} [{id}] · effort auto"),
                        &theme,
                    );
                }
            }
            push_choice_row(
                &mut lines,
                &mut row_lines,
                workers.len(),
                state.selected,
                "Skip and keep the current worker",
                &theme,
            );
            push_choice_row(
                &mut lines,
                &mut row_lines,
                workers.len() + 1,
                state.selected,
                "Continue",
                &theme,
            );
        }
        OnboardingStep::Community => {
            lines.push(Line::from("Follow @samfajreldines for Distill updates. Distill will only ask your browser to open the page."));
            lines.push(Line::from("It will not follow automatically, and this screen does not claim that you followed."));
            lines.push(Line::from(""));
            push_choice_row(
                &mut lines,
                &mut row_lines,
                0,
                state.selected,
                "Open X profile",
                &theme,
            );
            push_choice_row(
                &mut lines,
                &mut row_lines,
                1,
                state.selected,
                if state.x_requested {
                    "Finish"
                } else {
                    "Skip and finish"
                },
                &theme,
            );
            lines.push(Line::from(X_URL).style(Style::default().fg(theme.accent_user)));
        }
    }
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(1),
    };
    let footer = if content.width < 48 {
        "↑↓ · PgUp/PgDn · Enter · ← · Esc"
    } else if content.width < 68 {
        "↑↓/mouse · PgUp/PgDn · Enter · ← back · Esc"
    } else if compact {
        "↑/↓ select · PgUp/PgDn scroll · Enter continue · ← back · Esc close"
    } else {
        "↑↓/mouse · PgUp/PgDn · Enter · ← back · Esc · s skip"
    };

    let mut status_lines = Vec::new();
    if let Some(status) = &state.status {
        status_lines.push(Line::from(Span::styled(
            status.clone(),
            Style::default().fg(if state.status_is_error {
                theme.accent_error
            } else {
                theme.accent_success
            }),
        )));
    }
    if state.completion_pending {
        status_lines.push(Line::from(Span::styled(
            "Saving completion…",
            Style::default().fg(theme.gray_bright),
        )));
    }
    let status_paragraph = Paragraph::new(status_lines).wrap(Wrap { trim: true });
    let status_height = if state.status.is_some() || state.completion_pending {
        status_paragraph
            .line_count(content.width)
            .clamp(1, 3)
            .min(content.height as usize)
    } else {
        0
    } as u16;
    let body = Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: content.height.saturating_sub(status_height),
    };

    let wrap = Wrap { trim: true };
    let paragraph = Paragraph::new(lines.clone()).wrap(wrap);
    let mut line_starts = Vec::with_capacity(lines.len());
    let mut line_heights = Vec::with_capacity(lines.len());
    let mut total_height = 0usize;
    for line in &lines {
        line_starts.push(total_height);
        let height = Paragraph::new(line.clone())
            .wrap(wrap)
            .line_count(body.width)
            .max(1);
        line_heights.push(height);
        total_height = total_height.saturating_add(height);
    }
    let max_scroll = total_height.saturating_sub(body.height as usize);
    let mut scroll = state.scroll.min(max_scroll);
    if state.follow_selection {
        if let Some((_, line_index)) = row_lines.iter().find(|(index, _)| *index == state.selected)
        {
            let row_start = line_starts[*line_index];
            let row_end = row_start.saturating_add(line_heights[*line_index]);
            if row_start < scroll {
                scroll = row_start;
            } else if row_end > scroll.saturating_add(body.height as usize) {
                scroll = row_end.saturating_sub(body.height as usize);
            }
        }
    }
    state.scroll = scroll;
    paragraph
        .scroll((scroll.min(u16::MAX as usize) as u16, 0))
        .render(body, buf);

    if status_height > 0 {
        let status_area = Rect {
            x: content.x,
            y: content.y + body.height,
            width: content.width,
            height: status_height,
        };
        status_paragraph.render(status_area, buf);
    }
    let footer_area = Rect {
        x: content.x,
        y: content.y + content.height,
        width: content.width,
        height: inner.height.saturating_sub(content.height),
    };
    Paragraph::new(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.gray_dim),
    )))
    .render(footer_area, buf);

    // Hit regions use the same wrapped-line heights and scroll offset as the rendered body.
    for (index, line_index) in row_lines {
        let row_start = line_starts[line_index];
        let row_end = row_start.saturating_add(line_heights[line_index]);
        let visible_start = row_start.saturating_sub(scroll);
        let visible_end = row_end.saturating_sub(scroll).min(body.height as usize);
        if visible_start < visible_end {
            state.hit_rows.push((
                Rect {
                    x: body.x,
                    y: body.y + visible_start as u16,
                    width: body.width,
                    height: (visible_end - visible_start) as u16,
                },
                index,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_steps_have_stable_titles_and_progress() {
        let state = OnboardingState::new();
        assert_eq!(STEP_COUNT, 4);
        assert_eq!(state.step_number(), 1);
        assert_eq!(state.step.title(), "Make your AI budget go further");
        assert_eq!(OnboardingStep::Community.title(), "Stay in the loop");
    }

    #[test]
    fn escape_cancels_auth_without_closing_the_flow() {
        let mut state = OnboardingState::new();
        state.set_step(OnboardingStep::Connect);
        state.set_auth_started(Some(LoginProvider::OpenRouter));
        let command = state.handle_input(
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            )),
            0,
            0,
        );
        assert_eq!(
            command,
            Some(OnboardingCommand::CancelProviderLogin(
                LoginProvider::OpenRouter
            ))
        );
    }

    #[test]
    fn completion_error_does_not_mark_the_flow_done() {
        let mut state = OnboardingState::new();
        state.completion_pending = true;
        state.set_persistence_error("disk full");
        assert!(!state.completion_pending);
        assert!(state.status_is_error);
    }

    #[test]
    fn model_and_worker_messages_wait_for_persistence_results() {
        let mut state = OnboardingState::new();
        state.set_primary_model_pending();
        assert!(state.status.as_deref().unwrap().to_lowercase().contains("saving"));
        state.finish_setting_persistence(
            "default_model",
            true,
            "Reasoning model saved for planning and review.",
        );
        assert!(!state.status_is_error);
        assert!(state.status.as_deref().unwrap().contains("saved"));

        state.set_worker_model_pending();
        state.finish_setting_persistence(
            "tier_light",
            false,
            "Worker model was not saved: disk full. Try again.",
        );
        assert!(state.status_is_error);
        assert!(state.status.as_deref().unwrap().contains("not saved"));
    }

    #[test]
    fn large_catalog_scrolls_selected_row_and_derives_wrapped_mouse_hitbox() {
        let mut state = OnboardingState::new();
        state.set_step(OnboardingStep::Connect);
        let models = (0..40)
            .map(|index| {
                (
                    format!("model-{index}"),
                    format!(
                        "A deliberately long model label {index} that wraps in a narrow terminal"
                    ),
                )
            })
            .collect::<Vec<_>>();
        state.selected = 3 + models.len() - 1;
        let area = Rect::new(0, 0, 34, 14);
        let mut buffer = Buffer::empty(area);
        render_onboarding(
            &mut buffer,
            area,
            &mut state,
            false,
            &models,
            &[],
            None,
            None,
        );

        assert!(
            state.scroll > 0,
            "selected catalog entry must be brought into view"
        );
        let hit = state
            .hit_rows
            .iter()
            .find(|(_, index)| *index == state.selected)
            .map(|(rect, _)| *rect)
            .expect("selected row remains clickable after scrolling");
        assert!(
            hit.height >= 2,
            "wrapped row should expose its rendered height"
        );
        let command = state.handle_input(
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x,
                row: hit.y + hit.height - 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            models.len(),
            0,
        );
        assert_eq!(
            command,
            Some(OnboardingCommand::SelectModel(models.len() - 1))
        );

        let selected_scroll = state.scroll;
        state.handle_input(
            &Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            )),
            models.len(),
            0,
        );
        render_onboarding(
            &mut buffer,
            area,
            &mut state,
            false,
            &models,
            &[],
            None,
            None,
        );
        assert!(
            state.scroll < selected_scroll,
            "explicit page scrolling must not be forced back to the selected row"
        );

        let wide_area = Rect::new(0, 0, 80, 24);
        let mut wide_buffer = Buffer::empty(wide_area);
        state.set_step(OnboardingStep::Worker);
        render_onboarding(
            &mut wide_buffer,
            wide_area,
            &mut state,
            false,
            &[],
            &[],
            None,
            None,
        );
        let wide_rendered = wide_buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            wide_rendered.contains("Esc"),
            "80-column footer must keep Escape help visible"
        );
    }

    #[test]
    fn short_terminal_keeps_resize_instruction_visible() {
        let mut state = OnboardingState::new();
        let area = Rect::new(0, 0, 18, 6);
        let mut buffer = Buffer::empty(area);
        render_onboarding(&mut buffer, area, &mut state, false, &[], &[], None, None);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("larger"));
        assert!(rendered.contains("terminal"));
    }
}
