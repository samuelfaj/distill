use super::*;
use crate::theme::Theme;
use crate::views::modal_window::{ModalSizing, ModalWindowConfig, Shortcut};
use distill_shell::accounts::QuotaStatus;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

pub fn render_accounts_modal(
    buf: &mut Buffer,
    area: Rect,
    state: &mut AccountsModalState,
    compact: bool,
    theme: &Theme,
) {
    state.hits.clear();
    let shortcuts = [
        Shortcut {
            label: "Tab focus",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "↑↓",
            clickable: false,
            id: 0,
        },
    ];
    let sizing = ModalSizing {
        width_pct: 0.96,
        max_width: 150,
        min_width: 28,
        v_margin: 1,
        h_pad: 1,
        v_pad: 0,
        footer_lines: 1,
    }
    .with_compact(compact);
    let config = ModalWindowConfig {
        title: "Accounts & Auto Routing",
        tabs: None,
        shortcuts: &shortcuts,
        sizing,
        fold_info: None,
    };
    let Some(content) = mw::render_modal_window(buf, area, &mut state.window, &config, theme)
    else {
        return;
    };
    let area = content.content;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let status = if state.dirty() {
        "Unsaved routing"
    } else {
        "Routing saved"
    };
    Line::from(Span::styled(
        status,
        Style::default()
            .fg(if state.dirty() {
                theme.fuzzy_accent
            } else {
                theme.gray_bright
            })
            .add_modifier(Modifier::BOLD),
    ))
    .render(Rect::new(area.x, area.y, area.width, 1), buf);
    let mut y = 1;
    if matches!(state.busy, Some(AccountsRequest::Login(_))) {
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        line(
            buf,
            body,
            0,
            "Waiting for browser sign-in…",
            theme.fuzzy_accent,
        );
        if let Some(feedback) = &state.login_feedback {
            let text = format!(
                "Open this link to sign in:\n{}{}",
                feedback.authorization_url,
                feedback
                    .device_code
                    .as_ref()
                    .map(|code| format!("\nCode: {code}"))
                    .unwrap_or_default()
            );
            Paragraph::new(text)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .style(Style::default().fg(theme.text_primary))
                .render(
                    Rect::new(
                        body.x,
                        body.y + 1,
                        body.width,
                        body.height.saturating_sub(3),
                    ),
                    buf,
                );
            let mut y = body.height.saturating_sub(2);
            buttons(
                buf,
                body,
                &mut y,
                &[
                    ("o Open browser", Control::OpenUrl),
                    ("c Copy link", Control::CopyUrl),
                    ("Esc Cancel", Control::Cancel),
                ],
                state,
                theme,
            );
        } else {
            line(
                buf,
                body,
                1,
                "Finish sign-in in your browser.",
                theme.gray_bright,
            );
            let mut y = 2;
            buttons(
                buf,
                body,
                &mut y,
                &[("Esc Cancel login", Control::Cancel)],
                state,
                theme,
            );
        }
        line(
            buf,
            area,
            area.height.saturating_sub(1),
            state.message.as_deref().unwrap_or("Waiting for sign-in…"),
            theme.gray_bright,
        );
        return;
    }
    if matches!(state.mode, Mode::Browse) {
        if area.width < 68 {
            buttons(
                buf,
                area,
                &mut y,
                &[
                    ("Accounts", Control::Pane(Pane::Accounts)),
                    ("Auto Routing", Control::Pane(Pane::Routing)),
                ],
                state,
                theme,
            );
        }
        let bottom_height = if area.height >= 10 { 3 } else { 2 };
        let body = Rect::new(
            area.x,
            area.y + y,
            area.width,
            area.height.saturating_sub(y + bottom_height),
        );
        if area.width >= 68 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
                .split(body);
            render_accounts(buf, cols[0], state, theme);
            render_routes(buf, cols[1], state, theme);
        } else if state.pane == Pane::Accounts {
            render_accounts(buf, body, state, theme);
        } else {
            render_routes(buf, body, state, theme);
        }
        y = area.height.saturating_sub(bottom_height);
        buttons(
            buf,
            area,
            &mut y,
            &[("s Save", Control::Save), ("Esc Cancel", Control::Cancel)],
            state,
            theme,
        );
    } else {
        let body = Rect::new(
            area.x,
            area.y + y,
            area.width,
            area.height.saturating_sub(y + 1),
        );
        render_editor(buf, body, state, theme);
    }
    let message = match state.message.as_deref() {
        None | Some("Account changes save immediately; routing changes need Save.") => {
            if area.width < 68 {
                "Accounts save immediately."
            } else {
                "Accounts save immediately · Choose Auto in Main to use this profile."
            }
        }
        Some(message) => message,
    };
    line(
        buf,
        area,
        area.height.saturating_sub(1),
        message,
        if state.busy.is_some() {
            theme.fuzzy_accent
        } else if state.message.is_some()
            && state.message.as_deref()
                != Some("Account changes save immediately; routing changes need Save.")
        {
            theme.warning
        } else {
            theme.gray_bright
        },
    );
}
fn line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: impl Into<String>,
    color: ratatui::style::Color,
) {
    if y < area.height {
        Line::from(Span::styled(text.into(), Style::default().fg(color)))
            .render(Rect::new(area.x, area.y + y, area.width, 1), buf);
    }
}
fn buttons(
    buf: &mut Buffer,
    area: Rect,
    y: &mut u16,
    controls: &[(&str, Control)],
    state: &mut AccountsModalState,
    theme: &Theme,
) {
    let mut x = 0;
    for (label, control) in controls {
        let label = format!("[{label}]");
        let width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        if x > 0 && x + width > area.width {
            *y += 1;
            x = 0;
        }
        if *y >= area.height {
            return;
        }
        let rect = Rect::new(
            area.x + x,
            area.y + *y,
            width.min(area.width.saturating_sub(x)),
            1,
        );
        let selected = matches!(control, Control::Pane(p) if *p == state.pane);
        let primary = matches!(control, Control::Save) && state.dirty();
        let mut style = Style::default().fg(theme.gray_bright);
        if primary {
            style = style
                .fg(theme.fuzzy_accent)
                .bg(theme.bg_highlight)
                .add_modifier(Modifier::BOLD);
        } else if selected || matches!(control, Control::Add | Control::Login | Control::Edit) {
            style = style.fg(theme.fuzzy_accent).add_modifier(Modifier::BOLD);
        }
        Line::from(Span::styled(label, style)).render(rect, buf);
        state.hits.push((rect, *control));
        x += width + 1;
    }
    *y += 1;
}
fn panel(buf: &mut Buffer, area: Rect, title: &str, focused: bool, theme: &Theme) -> Rect {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            theme.fuzzy_accent
        } else {
            theme.selection_border
        }));
    if area.width < 35 {
        return area;
    }
    let inner = block.inner(area);
    block.render(area, buf);
    inner
}
fn render_accounts(buf: &mut Buffer, area: Rect, state: &mut AccountsModalState, theme: &Theme) {
    state.hits.push((area, Control::Pane(Pane::Accounts)));
    let inner = panel(buf, area, " Accounts ", state.pane == Pane::Accounts, theme);
    let mut y = 0;
    // Wrapping action rows stay outside the scrolling list.
    buttons(
        buf,
        inner,
        &mut y,
        &[
            ("a Add", Control::Add),
            ("l Login", Control::Login),
            ("m More", Control::More),
        ],
        state,
        theme,
    );
    let room = inner.height.saturating_sub(y) as usize;
    let row_height = if room >= 8 {
        4
    } else if room >= 3 {
        3
    } else {
        1
    };
    let visible = (room / row_height).max(1);
    keep_visible(&mut state.account_scroll, state.account_selected, visible);
    if state.accounts.is_empty() {
        line(
            buf,
            inner,
            y,
            if state.loaded {
                "No accounts yet. [a Add] to connect."
            } else {
                "Loading… [r Retry] if unavailable."
            },
            theme.gray_bright,
        );
    }
    for i in state.account_scroll..state.accounts.len().min(state.account_scroll + visible) {
        if y >= inner.height {
            break;
        }
        let a = &state.accounts[i];
        let selected = i == state.account_selected;
        let color = if selected && state.pane == Pane::Accounts {
            theme.fuzzy_accent
        } else {
            theme.text_primary
        };
        if selected {
            buf.set_style(
                Rect::new(inner.x, inner.y + y, inner.width, row_height.min(3) as u16),
                Style::default().bg(theme.bg_highlight),
            );
        }
        let connection = if a.connection_error.is_some() {
            "Connection error"
        } else if a.connected {
            "Connected"
        } else {
            "Disconnected"
        };
        line(
            buf,
            inner,
            y,
            format!(
                "{}{} · {}",
                if i == state.account_selected {
                    "› "
                } else {
                    "  "
                },
                provider_name(a.account.provider),
                a.account.label
            ),
            color,
        );
        if row_height > 1 {
            line(
                buf,
                inner,
                y + 1,
                format!(
                    "  {connection}{}",
                    a.email
                        .as_ref()
                        .map(|e| format!(" · {e}"))
                        .unwrap_or_default()
                ),
                if a.connection_error.is_some() {
                    theme.warning
                } else if a.connected {
                    theme.accent_success
                } else {
                    theme.gray_bright
                },
            );
            line(
                buf,
                inner,
                y + 2,
                format!("  {}", quota_label(a, state.busy.as_ref())),
                theme.gray_bright,
            );
        }
        state.hits.push((
            Rect::new(
                inner.x,
                inner.y + y,
                inner.width,
                (row_height as u16).min(inner.height - y),
            ),
            Control::Account(i),
        ));
        y += row_height as u16;
    }
}
pub(super) fn quota_label(account: &AccountStatus, busy: Option<&AccountsRequest>) -> String {
    if matches!(busy, Some(AccountsRequest::Refresh(id)) if *id == account.account.id) {
        return "Quota: refreshing…".into();
    }
    let Some(quota) = &account.quota else {
        return "Quota: unknown · r refresh".into();
    };
    let checked = quota.checked_at.format("%m-%d %H:%M UTC");
    match quota.status {
        QuotaStatus::Unsupported => "Quota: unsupported by provider".into(),
        QuotaStatus::Unavailable => format!("Quota: unavailable · checked {checked} · r retry"),
        QuotaStatus::Available => {
            let Some(value) = quota
                .remaining_percent
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
            else {
                return format!("Quota: unknown · checked {checked}");
            };
            let stale = !account.connected
                || account.connection_error.is_some()
                || quota.fetched_at.is_none_or(|t| {
                    let age = chrono::Utc::now().signed_duration_since(t);
                    age < chrono::Duration::zero() || age > chrono::Duration::minutes(5)
                });
            let cue = if stale {
                " (stale)".into()
            } else {
                let filled = (value / 20.0).floor() as usize;
                format!(" [{}{}]", "━".repeat(filled), "·".repeat(5 - filled))
            };
            format!("Quota: {value:.0}% remaining{cue} · {checked}")
        }
    }
}
fn render_routes(buf: &mut Buffer, area: Rect, state: &mut AccountsModalState, theme: &Theme) {
    state.hits.push((area, Control::Pane(Pane::Routing)));
    let inner = panel(
        buf,
        area,
        " Auto Routing ",
        state.pane == Pane::Routing,
        theme,
    );
    let mut y = 0;
    if inner.width < 35 || inner.height < 8 {
        buttons(
            buf,
            inner,
            &mut y,
            &[
                ("a Add", Control::Add),
                ("e Edit", Control::Edit),
                ("m More", Control::More),
            ],
            state,
            theme,
        );
        line(
            buf,
            inner,
            y,
            format!(
                "Auto {} · {}",
                if state.routing.enabled { "ON" } else { "OFF" },
                if state.routing.strategy == AutoStrategy::OrderedFirst {
                    "Ordered first"
                } else {
                    "Most remaining"
                }
            ),
            theme.gray_bright,
        );
        y += 1;
    } else {
        buttons(
            buf,
            inner,
            &mut y,
            &[
                (
                    if state.routing.enabled {
                        "Space ON"
                    } else {
                        "Space OFF"
                    },
                    Control::Toggle,
                ),
                (
                    if state.routing.strategy == AutoStrategy::OrderedFirst {
                        "t Ordered first"
                    } else {
                        "t Most remaining"
                    },
                    Control::Strategy,
                ),
            ],
            state,
            theme,
        );
        line(
            buf,
            inner,
            y,
            if state.routing.strategy == AutoStrategy::OrderedFirst {
                "Try eligible options in order."
            } else {
                "Highest known quota first; order breaks ties."
            },
            theme.gray_bright,
        );
        y += 1;
        buttons(
            buf,
            inner,
            &mut y,
            &[
                ("a Add", Control::Add),
                ("e Edit", Control::Edit),
                ("m More", Control::More),
            ],
            state,
            theme,
        );
    }
    let room = inner.height.saturating_sub(y) as usize;
    let row_height = if room >= 8 {
        4
    } else if room >= 3 {
        3
    } else {
        1
    };
    let visible = (room / row_height).max(1);
    keep_visible(&mut state.route_scroll, state.route_selected, visible);
    if state.routing.options.is_empty() {
        line(
            buf,
            inner,
            y,
            "No options. Add an account + Main/Worker.",
            theme.gray_bright,
        );
    }
    for i in state.route_scroll
        ..state
            .routing
            .options
            .len()
            .min(state.route_scroll + visible)
    {
        if y >= inner.height {
            break;
        }
        let option = &state.routing.options[i];
        let name = state
            .accounts
            .iter()
            .find(|a| a.account.id == option.account_id)
            .map(|a| {
                format!(
                    "{} · {}",
                    provider_name(a.account.provider),
                    a.account.label
                )
            })
            .unwrap_or_else(|| "Account no longer available".into());
        let selected = i == state.route_selected;
        let color = if selected && state.pane == Pane::Routing {
            theme.fuzzy_accent
        } else {
            theme.text_primary
        };
        if selected {
            buf.set_style(
                Rect::new(inner.x, inner.y + y, inner.width, row_height.min(3) as u16),
                Style::default().bg(theme.bg_highlight),
            );
        }
        line(
            buf,
            inner,
            y,
            format!(
                "{}{}. {name}",
                if i == state.route_selected {
                    "› "
                } else {
                    "  "
                },
                i + 1
            ),
            color,
        );
        if row_height > 1 {
            line(
                buf,
                inner,
                y + 1,
                format!("  Main: {}", model_name(&state.models, &option.main_model)),
                theme.text_primary,
            );
            line(
                buf,
                inner,
                y + 2,
                format!(
                    "  Worker: {}{}",
                    option
                        .worker_model
                        .as_ref()
                        .map(|m| model_name(&state.models, m))
                        .unwrap_or_else(|| "Same as Main".into()),
                    option
                        .worker_fallback_model
                        .as_ref()
                        .map(|m| format!(" → {}", model_name(&state.models, m)))
                        .unwrap_or_default()
                ),
                theme.gray_bright,
            );
        }
        state.hits.push((
            Rect::new(
                inner.x,
                inner.y + y,
                inner.width,
                (row_height as u16).min(inner.height - y),
            ),
            Control::Route(i),
        ));
        y += row_height as u16;
    }
}
fn keep_visible(scroll: &mut usize, selected: usize, visible: usize) {
    if selected < *scroll {
        *scroll = selected;
    }
    if selected >= *scroll + visible {
        *scroll = selected + 1 - visible;
    }
}
fn model_name(models: &[AccountModel], id: &str) -> String {
    models
        .iter()
        .find(|model| model.id == id)
        .map(|model| model.name.clone())
        .unwrap_or_else(|| id.into())
}
fn render_editor(buf: &mut Buffer, area: Rect, state: &mut AccountsModalState, theme: &Theme) {
    let mut y = 0;
    match &state.mode {
        Mode::Actions => {
            if state.pane == Pane::Accounts {
                if let Some(account) = state.accounts.get(state.account_selected) {
                    line(
                        buf,
                        area,
                        y,
                        format!(
                            "{} · {}",
                            provider_name(account.account.provider),
                            account.account.label
                        ),
                        theme.text_primary,
                    );
                    y += 1;
                }
                buttons(
                    buf,
                    area,
                    &mut y,
                    &[
                        ("n Rename", Control::Rename),
                        ("d Disconnect", Control::Logout),
                        ("x Remove", Control::Remove),
                        ("r Refresh quota", Control::Refresh),
                        ("i Account info", Control::Details),
                    ],
                    state,
                    theme,
                );
            } else {
                line(
                    buf,
                    area,
                    y,
                    if state.routing.options.is_empty() {
                        "Auto actions".into()
                    } else {
                        format!("Auto actions · Route {}", state.route_selected + 1)
                    },
                    theme.text_primary,
                );
                y += 1;
                buttons(
                    buf,
                    area,
                    &mut y,
                    &[
                        (
                            if state.routing.enabled {
                                "Space Disable Auto"
                            } else {
                                "Space Enable Auto"
                            },
                            Control::Toggle,
                        ),
                        ("t Change strategy", Control::Strategy),
                        ("K Move earlier", Control::Up),
                        ("J Move later", Control::Down),
                        ("x Remove option", Control::Remove),
                    ],
                    state,
                    theme,
                );
            }
            buttons(
                buf,
                area,
                &mut y,
                &[("Esc Back", Control::Cancel)],
                state,
                theme,
            );
            return;
        }
        Mode::Details { scroll } => {
            if let Some(account) = state.accounts.get(state.account_selected) {
                let text = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    provider_name(account.account.provider),
                    account.account.label,
                    account.email.as_deref().unwrap_or("Email unavailable"),
                    if account.connection_error.is_some() {
                        "Connection error · retry Login"
                    } else if account.connected {
                        "Connected"
                    } else {
                        "Disconnected"
                    },
                    quota_label(account, state.busy.as_ref())
                );
                Paragraph::new(text)
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .scroll((*scroll, 0))
                    .style(Style::default().fg(theme.text_primary))
                    .render(
                        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)),
                        buf,
                    );
            }
            let mut y = area.height.saturating_sub(1);
            buttons(
                buf,
                area,
                &mut y,
                &[("Esc Back", Control::Cancel)],
                state,
                theme,
            );
            return;
        }
        Mode::Add {
            provider,
            label,
            field,
        } => {
            line(
                buf,
                area,
                y,
                "Add account · Tab changes field",
                theme.gray_bright,
            );
            y += 1;
            field_line(
                buf,
                area,
                y,
                "Provider",
                provider_name(PROVIDERS[*provider]),
                *field == 0,
                theme,
            );
            state.hits.push((
                Rect::new(area.x, area.y + y, area.width, 1),
                Control::Field(0),
            ));
            y += 1;
            editor_line(buf, area, y, "Label", label, *field == 1, theme);
            state.hits.push((
                Rect::new(area.x, area.y + y, area.width, 1),
                Control::Field(1),
            ));
            y += 1;
            line(
                buf,
                area,
                y,
                "←/→ provider · Enter opens browser",
                theme.gray_bright,
            );
            y += 1;
        }
        Mode::Rename { label, .. } => {
            line(buf, area, y, "Rename selected account", theme.text_primary);
            y += 1;
            editor_line(buf, area, y, "Label", label, true, theme);
            y += 1;
        }
        Mode::Route { option, field, .. } => {
            let account = state
                .accounts
                .iter()
                .find(|a| a.account.id == option.account_id)
                .map(|a| {
                    format!(
                        "{} · {}",
                        provider_name(a.account.provider),
                        a.account.label
                    )
                })
                .unwrap_or_else(|| "Choose account".into());
            let values = [
                account,
                if option.main_model.is_empty() {
                    "Choose Main model".into()
                } else {
                    model_name(&state.models, &option.main_model)
                },
                option
                    .worker_model
                    .as_ref()
                    .map(|m| model_name(&state.models, m))
                    .unwrap_or_else(|| "Same as Main".into()),
                option
                    .worker_fallback_model
                    .as_ref()
                    .map(|m| model_name(&state.models, m))
                    .unwrap_or_else(|| "None".into()),
            ];
            for (i, (name, value)) in ["Account", "Main", "Worker", "Worker fallback"]
                .into_iter()
                .zip(values)
                .enumerate()
            {
                field_line(buf, area, y, name, &value, *field == i, theme);
                state.hits.push((
                    Rect::new(area.x, area.y + y, area.width, 1),
                    Control::Field(i),
                ));
                y += 1;
            }
            line(
                buf,
                area,
                y,
                "Main uses the selected account.",
                theme.gray_bright,
            );
            y += 1;
            let label = if *field == 4 {
                "› Enter Apply option"
            } else {
                "Enter Apply option"
            };
            buttons(
                buf,
                area,
                &mut y,
                &[(label, Control::Field(4)), ("Esc Cancel", Control::Cancel)],
                state,
                theme,
            );
            return;
        }
        Mode::Pick {
            kind,
            choices,
            query,
            selected,
        } => {
            let title = match kind {
                PickKind::Account => "Choose account",
                PickKind::Main => "Choose Main model",
                PickKind::Worker => "Choose Worker model",
                PickKind::WorkerFallback => "Choose Worker fallback",
            };
            line(buf, area, y, title, theme.fuzzy_accent);
            y += 1;
            editor_line(buf, area, y, "Find", query, true, theme);
            y += 1;
            let filtered: Vec<usize> = choices
                .iter()
                .enumerate()
                .filter(|(_, (label, id))| {
                    format!("{label} {id}")
                        .to_lowercase()
                        .contains(&query.text().to_lowercase())
                })
                .map(|(i, _)| i)
                .collect();
            let visible = area.height.saturating_sub(y + 1).max(1) as usize;
            let start = selected.saturating_sub(visible - 1);
            if filtered.is_empty() {
                line(
                    buf,
                    area,
                    y,
                    "No matches. Backspace to edit search.",
                    theme.gray_bright,
                );
            }
            for (position, i) in filtered.iter().enumerate().skip(start).take(visible) {
                let (label, id) = &choices[*i];
                let text = if matches!(kind, PickKind::Account) || id.is_empty() {
                    label.clone()
                } else {
                    format!("{label} · {id}")
                };
                line(
                    buf,
                    area,
                    y,
                    format!("{}{text}", if position == *selected { "› " } else { "  " }),
                    if position == *selected {
                        theme.fuzzy_accent
                    } else {
                        theme.text_primary
                    },
                );
                if y < area.height {
                    state.hits.push((
                        Rect::new(area.x, area.y + y, area.width, 1),
                        Control::Choose(*i),
                    ));
                }
                y += 1;
            }
            let mut bottom = area.height.saturating_sub(1);
            buttons(
                buf,
                area,
                &mut bottom,
                &[("Esc Back", Control::Cancel)],
                state,
                theme,
            );
            return;
        }
        Mode::Confirm { prompt, .. } => {
            Paragraph::new(prompt.as_str())
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().fg(theme.warning))
                .render(
                    Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)),
                    buf,
                );
            y = area.height.saturating_sub(1);
        }
        Mode::Discard => {
            line(
                buf,
                area,
                y,
                "Discard unsaved routing changes?",
                theme.warning,
            );
            y += 1;
        }
        Mode::Browse => return,
    }
    let accept = if matches!(state.mode, Mode::Confirm { .. } | Mode::Discard) {
        "Enter Confirm"
    } else {
        "Enter Continue"
    };
    buttons(
        buf,
        area,
        &mut y,
        &[(accept, Control::Accept), ("Esc Cancel", Control::Cancel)],
        state,
        theme,
    );
}
fn field_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    label: &str,
    value: &str,
    focused: bool,
    theme: &Theme,
) {
    line(
        buf,
        area,
        y,
        format!("{}{label}: {value}", if focused { "› " } else { "  " }),
        if focused {
            theme.fuzzy_accent
        } else {
            theme.text_primary
        },
    );
}
fn editor_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    label: &str,
    editor: &LineEditor,
    focused: bool,
    theme: &Theme,
) {
    if y >= area.height {
        return;
    }
    let prefix = format!("{}{label}: ", if focused { "› " } else { "  " });
    let width = area.width.saturating_sub(prefix.width() as u16) as usize;
    let viewport = editor.viewport(width);
    line(
        buf,
        area,
        y,
        format!(
            "{prefix}{}",
            editor
                .text()
                .get(viewport.visible_byte_range)
                .unwrap_or_default()
        ),
        if focused {
            theme.fuzzy_accent
        } else {
            theme.text_primary
        },
    );
    if focused {
        let x = area.x + prefix.width() as u16 + viewport.cursor_display_column as u16;
        if x < area.x + area.width
            && let Some(cell) = buf.cell_mut((x, area.y + y))
        {
            cell.set_style(theme.block_cursor_over(theme.bg_highlight));
        }
    }
}
