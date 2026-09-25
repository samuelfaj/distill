//! Four-step first-run onboarding for the Distill TUI.
//!
//! The state is deliberately independent from authentication and model storage. The host
//! translates the small commands below into the existing login, `/model`, `/reasoning-model`, persistence,
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
    Reasoning,
    Community,
}

impl OnboardingStep {
    fn index(self) -> usize {
        match self {
            Self::Budget => 0,
            Self::Connect => 1,
            Self::Reasoning => 2,
            Self::Community => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        match index.min(STEP_COUNT - 1) {
            0 => Self::Budget,
            1 => Self::Connect,
            2 => Self::Reasoning,
            _ => Self::Community,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Budget => "Welcome to Distill",
            Self::Connect => "Connect a provider",
            Self::Reasoning => "Add a reasoning model",
            Self::Community => "You're ready",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::Budget => "A few quick choices. You can change everything later.",
            Self::Connect => "Sign in, or keep your current model setup.",
            Self::Reasoning => "Optional: use a second model for planning and review.",
            Self::Community => "Setup is complete. Follow updates if you'd like.",
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
    CopyAuthUrl(String),
    SelectModel(usize),
    SelectReasoning(usize),
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
    auth_url: Option<String>,
    pub x_requested: bool,
    pub pulse: u8,
    pub scroll: usize,
    pub status_scroll: u16,
    follow_selection: bool,
    hit_rows: Vec<(Rect, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingSetting {
    MainModel,
    ReasoningModel,
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
            auth_url: None,
            x_requested: false,
            pulse: 0,
            scroll: 0,
            status_scroll: 0,
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
        self.auth_url = None;
        self.status_scroll = 0;
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
        self.auth_url = None;
        self.status = Some(message.into());
        self.status_is_error = !success;
    }

    pub fn set_info(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_is_error = false;
    }

    pub fn set_auth_browser_fallback(&mut self, url: &str) {
        self.auth_url = Some(url.to_owned());
        let remote_hint = match self.auth_provider {
            Some(LoginProvider::ChatGpt) => {
                "For a VPS, cancel, run `distill login --chatgpt --device-auth`, then restart Distill."
            }
            Some(LoginProvider::OpenRouter) => {
                "For a VPS, set OPENROUTER_API_KEY before starting Distill."
            }
            None => {
                "For a VPS, cancel and run `distill login --device-auth` in a shell, then restart Distill."
            }
        };
        self.status = Some(format!(
            "The browser could not open automatically. Press c to copy the full URL, then open it on a device with a browser:\n{url}\n{remote_hint}"
        ));
        self.status_scroll = 0;
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

    pub fn set_main_model_pending(&mut self) {
        self.setting_pending = Some(PendingSetting::MainModel);
        self.status = Some("Main model updated; saving it…".to_owned());
        self.status_is_error = false;
    }

    pub fn set_reasoning_model_pending(&mut self) {
        self.setting_pending = Some(PendingSetting::ReasoningModel);
        self.status = Some("Saving the reasoning model…".to_owned());
        self.status_is_error = false;
    }

    pub fn finish_setting_persistence(
        &mut self,
        key: &str,
        success: bool,
        message: impl Into<String>,
    ) {
        let expected = match key {
            "default_model" => PendingSetting::MainModel,
            "reasoning_model" => PendingSetting::ReasoningModel,
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
        if self.x_requested {
            return;
        }
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
        self.status_scroll = 0;
        self.follow_selection = true;
        self.status = None;
        self.status_is_error = false;
    }

    pub fn enter_community(&mut self) -> Option<OnboardingCommand> {
        if self.step != OnboardingStep::Community || self.x_requested {
            return None;
        }
        self.set_browser_requested();
        Some(OnboardingCommand::OpenX)
    }

    pub fn mark_community_open_requested(&mut self) {
        self.set_browser_requested();
    }

    fn item_count(&self, model_count: usize, reasoning_count: usize) -> usize {
        match self.step {
            OnboardingStep::Budget => 1,
            OnboardingStep::Connect => 4 + model_count,
            OnboardingStep::Reasoning => reasoning_count + 2,
            OnboardingStep::Community => 1,
        }
    }

    fn activate(
        &mut self,
        model_count: usize,
        reasoning_count: usize,
    ) -> Option<OnboardingCommand> {
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
                    self.set_step(OnboardingStep::Reasoning);
                    Some(OnboardingCommand::Continue)
                }
            },
            OnboardingStep::Reasoning => {
                if self.selected < reasoning_count {
                    Some(OnboardingCommand::SelectReasoning(self.selected))
                } else {
                    self.set_step(OnboardingStep::Community);
                    self.enter_community().or(Some(OnboardingCommand::Continue))
                }
            }
            OnboardingStep::Community => Some(OnboardingCommand::Complete),
        }
    }

    pub fn handle_input(
        &mut self,
        ev: &Event,
        model_count: usize,
        reasoning_count: usize,
    ) -> Option<OnboardingCommand> {
        if self.completion_pending || self.setting_pending.is_some() {
            return None;
        }
        if let Event::Mouse(mouse) = ev {
            if self.auth_pending {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.status_scroll = self.status_scroll.saturating_sub(5);
                    }
                    MouseEventKind::ScrollDown => {
                        self.status_scroll = self.status_scroll.saturating_add(5);
                    }
                    _ => {}
                }
                return None;
            }
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
                return self.activate(model_count, reasoning_count);
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
            match key.code {
                KeyCode::PageUp => self.status_scroll = self.status_scroll.saturating_sub(5),
                KeyCode::PageDown => self.status_scroll = self.status_scroll.saturating_add(5),
                KeyCode::Char('c') => {
                    return self.auth_url.clone().map(OnboardingCommand::CopyAuthUrl);
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up => {
                let count = self.item_count(model_count, reasoning_count);
                self.selected = if self.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                self.follow_selection = true;
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                let count = self.item_count(model_count, reasoning_count).max(1);
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
                self.selected = self
                    .item_count(model_count, reasoning_count)
                    .saturating_sub(1);
                self.follow_selection = true;
                None
            }
            KeyCode::Left | KeyCode::Backspace => {
                if self.step.index() > 0 {
                    self.set_step(OnboardingStep::from_index(self.step.index() - 1));
                }
                None
            }
            KeyCode::Right | KeyCode::Enter => self.activate(model_count, reasoning_count),
            KeyCode::Char('s') => match self.step {
                OnboardingStep::Budget | OnboardingStep::Connect => {
                    self.set_step(OnboardingStep::from_index(self.step.index() + 1));
                    Some(OnboardingCommand::Continue)
                }
                OnboardingStep::Reasoning => {
                    self.set_step(OnboardingStep::Community);
                    self.enter_community().or(Some(OnboardingCommand::Continue))
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
    let marker = if index == selected { "◉ " } else { "○ " };
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
    reasoning_options: &[(String, String)],
    current_reasoning: Option<&str>,
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
    let title = Line::from(vec![
        Span::styled(
            " DISTILL ",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("·", Style::default().fg(theme.gray_dim)),
        Span::styled(
            format!(" {}/{} ", state.step_number(), STEP_COUNT),
            Style::default().fg(theme.accent_user),
        ),
        Span::styled("·", Style::default().fg(theme.gray_dim)),
        Span::styled(
            format!(" {} ", state.step.title()),
            Style::default().fg(theme.text_primary),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.gray_dim))
        .style(Style::default().bg(theme.bg_base));
    let inner = block.inner(popup);
    block.render(popup, buf);

    let mut intro_lines = Vec::new();
    let mut option_lines = Vec::new();
    let mut row_lines = Vec::new();
    intro_lines.push(Line::from(Span::styled(
        format!(
            "{}  {}",
            if (state.pulse / 6).is_multiple_of(2) {
                "◆"
            } else {
                "◇"
            },
            state.step.title()
        ),
        Style::default()
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD),
    )));
    intro_lines
        .push(Line::from(state.step.subtitle()).style(Style::default().fg(theme.text_secondary)));
    match state.step {
        OnboardingStep::Budget => {
            let mark = if (state.pulse / 6).is_multiple_of(2) {
                "✦"
            } else {
                "·"
            };
            intro_lines.push(Line::from(Span::styled(
                format!("{mark} One workspace for your AI models."),
                Style::default()
                    .fg(theme.accent_success)
                    .add_modifier(Modifier::BOLD),
            )));
            intro_lines.push(Line::from("Choose the models and providers you already use. You can change these settings anytime."));
            intro_lines.push(Line::from("OpenRouter adds provider and pricing choices; it does not automatically reduce token usage."));
            intro_lines.push(Line::from(""));
            push_choice_row(
                &mut option_lines,
                &mut row_lines,
                0,
                state.selected,
                "Continue",
                &theme,
            );
        }
        OnboardingStep::Connect => {
            intro_lines.push(Line::from(
                "Choose a provider to sign in, then select the model Distill should use by default.",
            ));
            intro_lines.push(Line::from("On a VPS: `distill login --device-auth` for Grok, `distill login --chatgpt --device-auth` for ChatGPT, or `OPENROUTER_API_KEY` for OpenRouter."));
            intro_lines.push(Line::from("Provider sign-in returns here; your current model stays unchanged until you select one."));
            intro_lines.push(Line::from(""));
            let auth = provider_auth.unwrap_or_default();
            push_choice_row(
                &mut option_lines,
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
                &mut option_lines,
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
                &mut option_lines,
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
                    &mut option_lines,
                    &mut row_lines,
                    index + 3,
                    state.selected,
                    format!("Main model: {name} [{id}]"),
                    &theme,
                );
            }
            push_choice_row(
                &mut option_lines,
                &mut row_lines,
                3 + models.len(),
                state.selected,
                "Continue without changing the main model",
                &theme,
            );
        }
        OnboardingStep::Reasoning => {
            intro_lines.push(Line::from(
                "The main model runs every step. An optional reasoning model plans and reviews the steps the main model cannot do alone.",
            ));
            if let Some(current) = current_reasoning.filter(|value| !value.is_empty()) {
                intro_lines.push(Line::from(format!(
                    "Current reasoning model: {current} (Skip keeps it)"
                )));
            }
            if reasoning_options.is_empty() {
                intro_lines.push(Line::from("No other model is available in the current catalog. Skip keeps the existing configuration."));
            } else {
                for (index, (id, name)) in reasoning_options.iter().enumerate() {
                    push_choice_row(
                        &mut option_lines,
                        &mut row_lines,
                        index,
                        state.selected,
                        format!("{name}  [{id}]"),
                        &theme,
                    );
                }
            }
            push_choice_row(
                &mut option_lines,
                &mut row_lines,
                reasoning_options.len(),
                state.selected,
                "Skip and keep the current reasoning model",
                &theme,
            );
            push_choice_row(
                &mut option_lines,
                &mut row_lines,
                reasoning_options.len() + 1,
                state.selected,
                "Continue",
                &theme,
            );
        }
        OnboardingStep::Community => {
            intro_lines.push(Line::from("Follow @samfajreldines for Distill updates."));
            intro_lines.push(Line::from("You can finish setup after the page opens."));
            intro_lines.push(Line::from(""));
            push_choice_row(
                &mut option_lines,
                &mut row_lines,
                0,
                state.selected,
                "Finish",
                &theme,
            );
            intro_lines.push(Line::from(X_URL).style(Style::default().fg(theme.accent_user)));
        }
    }
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(1),
    };
    let key_style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight)
        .add_modifier(Modifier::BOLD);
    let footer_line = if content.width < 36 && state.auth_pending {
        Line::from("c copy · PgUp/Dn scroll · Esc")
    } else if content.width < 36 {
        Line::from("↑↓ move · Enter · Esc")
    } else if state.auth_pending {
        Line::from(vec![
            Span::styled(" c ", key_style),
            Span::raw(" Copy URL   "),
            Span::styled("PgUp/PgDn", key_style),
            Span::raw(" Scroll   "),
            Span::styled("Esc", key_style),
            Span::raw(" Cancel"),
        ])
    } else if compact {
        Line::from(vec![
            Span::styled(" ↑↓ ", key_style),
            Span::raw(" Navigate   "),
            Span::styled("Enter", key_style),
            Span::raw(" Select   "),
            Span::styled("Esc", key_style),
            Span::raw(" Close"),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ↑↓ ", key_style),
            Span::raw(" Navigate   "),
            Span::styled("Enter", key_style),
            Span::raw(" Select   "),
            Span::styled("←", key_style),
            Span::raw(" Back   "),
            Span::styled("Esc", key_style),
            Span::raw(" Close   "),
            Span::styled("s", key_style),
            Span::raw(" Skip"),
        ])
    };
    let footer_area = Rect {
        x: content.x,
        y: content.bottom().saturating_sub(1),
        width: content.width,
        height: 1,
    };
    Paragraph::new(footer_line).render(footer_area, buf);

    let mut status_lines = Vec::new();
    if let Some(status) = &state.status {
        let style = Style::default().fg(if state.status_is_error {
            theme.accent_error
        } else {
            theme.accent_success
        });
        status_lines.extend(
            status
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), style))),
        );
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
            .clamp(1, 8)
            .min((content.height / 3).max(1) as usize)
    } else {
        0
    } as u16;
    let max_status_scroll = status_paragraph
        .line_count(content.width)
        .saturating_sub(status_height as usize)
        .min(u16::MAX as usize) as u16;
    state.status_scroll = state.status_scroll.min(max_status_scroll);
    if status_height > 0 {
        let status_area = Rect {
            x: content.x,
            y: footer_area.y.saturating_sub(status_height),
            width: content.width,
            height: status_height,
        };
        status_paragraph
            .scroll((state.status_scroll, 0))
            .render(status_area, buf);
    }

    let main_area = Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: footer_area
            .y
            .saturating_sub(content.y)
            .saturating_sub(status_height),
    };
    let compact_intro = main_area.width < 40 || main_area.height < 12;
    let intro_lines = if compact_intro {
        vec![Line::from(match state.step {
            OnboardingStep::Budget => "Providers and models can be changed later.",
            OnboardingStep::Connect => "VPS login: use device auth or an API key.",
            OnboardingStep::Reasoning => "Optional: a second model for planning and review.",
            OnboardingStep::Community => "Follow updates or finish setup.",
        })]
    } else {
        intro_lines
    };
    let intro_width = main_area.width.saturating_sub(4).max(1);
    let intro_paragraph = Paragraph::new(intro_lines).wrap(Wrap { trim: true });
    let intro_height = intro_paragraph
        .line_count(intro_width)
        .saturating_add(2)
        .clamp(3, 8)
        .min(main_area.height.saturating_sub(5).max(3) as usize) as u16;
    let intro_area = Rect {
        x: main_area.x,
        y: main_area.y,
        width: main_area.width,
        height: intro_height,
    };
    let intro_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.gray_dim))
        .style(Style::default().bg(theme.bg_base));
    let intro_inner = intro_block.inner(intro_area);
    intro_block.render(intro_area, buf);
    intro_paragraph.render(intro_inner, buf);

    let choices_area = Rect {
        x: main_area.x,
        y: intro_area.bottom().saturating_add(1),
        width: main_area.width,
        height: main_area
            .bottom()
            .saturating_sub(intro_area.bottom().saturating_add(1)),
    };
    let choices_title = match state.step {
        OnboardingStep::Budget => "Get started",
        OnboardingStep::Connect => "Select a provider or model",
        OnboardingStep::Reasoning => "Select a reasoning model",
        OnboardingStep::Community => "Finish setup",
    };
    let choices_block = Block::default()
        .title(choices_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_user))
        .style(Style::default().bg(theme.bg_base));
    let choices_inner = choices_block.inner(choices_area);
    choices_block.render(choices_area, buf);

    let wrap = Wrap { trim: true };
    let paragraph = Paragraph::new(option_lines.clone()).wrap(wrap);
    let mut line_starts = Vec::with_capacity(option_lines.len());
    let mut line_heights = Vec::with_capacity(option_lines.len());
    let mut total_height = 0usize;
    for line in &option_lines {
        line_starts.push(total_height);
        let height = Paragraph::new(line.clone())
            .wrap(wrap)
            .line_count(choices_inner.width)
            .max(1);
        line_heights.push(height);
        total_height = total_height.saturating_add(height);
    }
    let max_scroll = total_height.saturating_sub(choices_inner.height as usize);
    let mut scroll = state.scroll.min(max_scroll);
    if state.follow_selection {
        if let Some((_, line_index)) = row_lines.iter().find(|(index, _)| *index == state.selected)
        {
            let row_start = line_starts[*line_index];
            let row_end = row_start.saturating_add(line_heights[*line_index]);
            let viewport_height = choices_inner.height as usize;
            if row_end.saturating_sub(row_start) > viewport_height {
                scroll = row_start;
            } else if row_start < scroll {
                scroll = row_start;
            } else if row_end > scroll.saturating_add(viewport_height) {
                scroll = row_end.saturating_sub(viewport_height);
            }
        }
    }
    state.scroll = scroll;
    paragraph
        .scroll((scroll.min(u16::MAX as usize) as u16, 0))
        .render(choices_inner, buf);

    // Hit regions use the same wrapped-line heights and scroll offset as the rendered list.
    for (index, line_index) in row_lines {
        let row_start = line_starts[line_index];
        let row_end = row_start.saturating_add(line_heights[line_index]);
        let visible_start = row_start.saturating_sub(scroll);
        let visible_end = row_end
            .saturating_sub(scroll)
            .min(choices_inner.height as usize);
        if visible_start < visible_end {
            state.hit_rows.push((
                Rect {
                    x: choices_inner.x,
                    y: choices_inner.y + visible_start as u16,
                    width: choices_inner.width,
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
        assert_eq!(state.step.title(), "Welcome to Distill");
        assert_eq!(OnboardingStep::Community.title(), "You're ready");
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
    fn browser_fallback_explains_vps_login_alternatives() {
        let mut state = OnboardingState::new();
        state.set_auth_started(Some(LoginProvider::ChatGpt));
        state.set_auth_browser_fallback("https://example.test/login");
        assert!(
            state
                .status
                .as_deref()
                .unwrap()
                .contains("distill login --chatgpt --device-auth")
        );

        state.set_auth_started(Some(LoginProvider::OpenRouter));
        state.set_auth_browser_fallback("https://example.test/login");
        assert!(
            state
                .status
                .as_deref()
                .unwrap()
                .contains("OPENROUTER_API_KEY")
        );

        state.set_auth_started(None);
        state.set_auth_browser_fallback("https://example.test/login");
        assert!(
            state
                .status
                .as_deref()
                .unwrap()
                .contains("distill login --device-auth")
        );
        assert!(state.status.as_deref().unwrap().contains("Press c to copy"));
        assert_eq!(
            state.handle_input(
                &Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('c'),
                    crossterm::event::KeyModifiers::NONE,
                )),
                0,
                0,
            ),
            Some(OnboardingCommand::CopyAuthUrl(
                "https://example.test/login".to_owned()
            ))
        );
    }

    #[test]
    fn browser_fallback_keeps_the_full_url_visible_in_a_normal_terminal() {
        let mut state = OnboardingState::new();
        state.set_step(OnboardingStep::Connect);
        state.set_auth_started(Some(LoginProvider::ChatGpt));
        let url = format!("https://example.test/{}-end", "x".repeat(2_000));
        state.set_auth_browser_fallback(&url);

        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        render_onboarding(&mut buffer, area, &mut state, false, &[], &[], None, None);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("The browser could not open automatically"));
        assert!(rendered.contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));

        let mut status = String::new();
        for _ in 0..10 {
            state.handle_input(
                &Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::PageDown,
                    crossterm::event::KeyModifiers::NONE,
                )),
                0,
                0,
            );
            render_onboarding(&mut buffer, area, &mut state, false, &[], &[], None, None);
            status.push_str(
                &buffer
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .replace(' ', ""),
            );
        }
        assert!(status.contains("-end"));
        assert!(
            status.contains("distilllogin--chatgpt--device-auth"),
            "tail render missing remote hint at offset {}: {status}",
            state.status_scroll
        );

        state.status_scroll = 0;
        state.handle_input(
            &Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 40,
                row: 12,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            0,
            0,
        );
        assert!(
            state.status_scroll > 0,
            "mouse wheel should scroll auth details"
        );
        render_onboarding(&mut buffer, area, &mut state, false, &[], &[], None, None);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
    }

    #[test]
    fn keyboard_flow_reaches_model_selection_and_can_finish_without_browser_actions() {
        let mut state = OnboardingState::new();
        let enter = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(
            state.handle_input(&enter, 1, 1),
            Some(OnboardingCommand::Continue)
        );
        assert_eq!(state.step, OnboardingStep::Connect);

        state.selected = 3;
        assert_eq!(
            state.handle_input(&enter, 1, 1),
            Some(OnboardingCommand::SelectModel(0))
        );
        state.set_main_model_pending();
        state.finish_setting_persistence("default_model", true, "Main model saved.");
        assert!(!state.status_is_error);

        state.set_step(OnboardingStep::Reasoning);
        state.selected = 1;
        assert_eq!(
            state.handle_input(&enter, 0, 1),
            Some(OnboardingCommand::OpenX)
        );
        assert_eq!(state.step, OnboardingStep::Community);
        assert_eq!(state.selected, 0);
        assert_eq!(state.item_count(0, 0), 1);
        assert_eq!(
            state.handle_input(&enter, 0, 0),
            Some(OnboardingCommand::Complete)
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
    fn main_and_reasoning_messages_wait_for_persistence_results() {
        let mut state = OnboardingState::new();
        state.set_main_model_pending();
        assert!(
            state
                .status
                .as_deref()
                .unwrap()
                .to_lowercase()
                .contains("saving")
        );
        state.finish_setting_persistence(
            "default_model",
            true,
            "Main model saved. Continue when ready.",
        );
        assert!(!state.status_is_error);
        assert!(state.status.as_deref().unwrap().contains("saved"));

        state.set_reasoning_model_pending();
        state.finish_setting_persistence(
            "reasoning_model",
            false,
            "Reasoning model was not saved: disk full. Try again.",
        );
        assert!(state.status_is_error);
        assert!(state.status.as_deref().unwrap().contains("not saved"));
    }

    #[test]
    fn every_step_keeps_a_compact_card_and_exit_hint_in_a_small_terminal() {
        let area = Rect::new(0, 0, 34, 14);
        let expected_copy = [
            ("Providers and models", "Get started"),
            ("VPS login", "Select a provider"),
            ("Optional: a second model", "Select a reasoning"),
            ("Follow updates", "Finish setup"),
        ];
        for (index, step) in [
            OnboardingStep::Budget,
            OnboardingStep::Connect,
            OnboardingStep::Reasoning,
            OnboardingStep::Community,
        ]
        .into_iter()
        .enumerate()
        {
            let mut state = OnboardingState::new();
            state.set_step(step);
            let mut buffer = Buffer::empty(area);
            render_onboarding(&mut buffer, area, &mut state, false, &[], &[], None, None);
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                rendered.contains(expected_copy[index].0),
                "step {step:?}: {rendered}"
            );
            assert!(
                rendered.contains("Esc"),
                "exit hint missing for {step:?}: {rendered}"
            );
            assert!(
                rendered.contains(expected_copy[index].1),
                "choice panel missing for {step:?}: {rendered}"
            );
        }
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
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("◉"),
            "selected radio marker must remain visible"
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
        state.set_step(OnboardingStep::Reasoning);
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
