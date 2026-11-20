//! Exact-account management and a routing draft; credentials remain in AccountStore.
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use distill_shell::accounts::{AccountStatus, AutoOption, AutoRouting, AutoStrategy, Provider};
use ratatui::layout::Rect;
use tokio_util::sync::CancellationToken;

use crate::input::line_editor::LineEditor;
use crate::views::modal_window::{self as mw, ModalWindowState};

#[path = "accounts_modal/render.rs"]
mod render;
pub use render::render_accounts_modal;
#[cfg(test)]
#[path = "accounts_modal/tests.rs"]
mod tests;

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
const PROVIDERS: [Provider; 4] = [
    Provider::Chatgpt,
    Provider::Claude,
    Provider::Grok,
    Provider::Openrouter,
];

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Chatgpt => "ChatGPT",
        Provider::Claude => "Claude",
        Provider::Grok => "Grok",
        Provider::Openrouter => "OpenRouter",
    }
}

#[derive(Debug, Clone)]
pub enum AccountsRequest {
    Load,
    Add {
        id: String,
        provider: Provider,
        label: String,
    },
    Rename {
        id: String,
        label: String,
    },
    Login(String),
    Logout(String),
    Remove(String),
    Refresh(String),
    Save(AutoRouting),
}
impl AccountsRequest {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Load => "Loading accounts…",
            Self::Add { .. } => "Adding account…",
            Self::Rename { .. } => "Renaming account…",
            Self::Login(_) => "Opening browser. Complete sign-in, then return here. Esc cancels.",
            Self::Logout(_) => "Disconnecting account…",
            Self::Remove(_) => "Removing account…",
            Self::Refresh(_) => "Refreshing quota…",
            Self::Save(_) => "Saving routing…",
        }
    }
}

/// Public catalog metadata only; an absent/unknown provider remains unverified.
#[derive(Debug, Clone)]
pub struct AccountModel {
    pub id: String,
    pub name: String,
    pub provider: Option<Provider>,
}
fn account_models(
    models: impl IntoIterator<Item = agent_client_protocol::ModelInfo>,
) -> Vec<AccountModel> {
    models
        .into_iter()
        .filter(|info| info.model_id.0.as_ref() != "auto")
        .map(|info| {
            let provider = info
                .meta
                .as_ref()
                .and_then(|meta| meta.get(distill_shell::agent::config::ACCOUNT_PROVIDER_META_KEY))
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<Provider>().ok());
            AccountModel {
                id: info.model_id.0.to_string(),
                name: info.name,
                provider,
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct AccountsData {
    pub accounts: Vec<AccountStatus>,
    pub routing: AutoRouting,
    pub catalog: Option<agent_client_protocol::SessionModelState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Accounts,
    Routing,
}
#[derive(Debug)]
pub(super) enum Mode {
    Browse,
    Actions,
    Details {
        scroll: u16,
    },
    Add {
        provider: usize,
        label: LineEditor,
        field: usize,
    },
    Rename {
        id: String,
        label: LineEditor,
    },
    Route {
        original: Option<usize>,
        option: AutoOption,
        field: usize,
    },
    Pick {
        kind: PickKind,
        choices: Vec<(String, String)>,
        query: LineEditor,
        selected: usize,
    },
    Confirm {
        prompt: String,
        request: AccountsRequest,
    },
    Discard,
}
#[derive(Debug, Clone, Copy)]
pub(super) enum PickKind {
    Account,
    Main,
    Worker,
    WorkerFallback,
}
#[derive(Debug, Clone, Copy)]
pub(super) enum Control {
    Account(usize),
    Route(usize),
    Pane(Pane),
    Add,
    Rename,
    Login,
    Logout,
    Remove,
    Refresh,
    Toggle,
    Strategy,
    Edit,
    Up,
    Down,
    Save,
    Cancel,
    Field(usize),
    Choose(usize),
    Accept,
    More,
    Details,
    OpenUrl,
    CopyUrl,
}
#[derive(Debug)]
pub enum AccountsOutcome {
    Changed,
    Close,
    Request(AccountsRequest),
    OpenUrl(String),
    CopyUrl(String),
}

pub struct AccountsModalState {
    pub window: ModalWindowState,
    pub instance: u64,
    pub accounts: Vec<AccountStatus>,
    pub routing: AutoRouting,
    pub saved: AutoRouting,
    pub loaded: bool,
    pub auto_activation_pending: bool,
    pub message: Option<String>,
    pub busy: Option<AccountsRequest>,
    pub cancel: Option<CancellationToken>,
    pub request_seq: u64,
    pub models: Vec<AccountModel>,
    pub login_feedback: Option<distill_shell::accounts::AccountLoginFeedback>,
    pub(super) pane: Pane,
    pub(super) account_selected: usize,
    pub(super) route_selected: usize,
    pub(super) mode: Mode,
    pub(super) route_editor: Option<Box<Mode>>,
    pub(super) hits: Vec<(Rect, Control)>,
    pub(super) account_scroll: usize,
    pub(super) route_scroll: usize,
}
impl Drop for AccountsModalState {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}
impl AccountsModalState {
    pub fn new(models: Vec<agent_client_protocol::ModelInfo>) -> Self {
        Self {
            window: ModalWindowState::new(),
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            accounts: vec![],
            routing: AutoRouting::default(),
            saved: AutoRouting::default(),
            loaded: false,
            auto_activation_pending: false,
            message: None,
            busy: None,
            cancel: None,
            request_seq: 0,
            models: account_models(models),
            login_feedback: None,
            pane: Pane::Accounts,
            account_selected: 0,
            route_selected: 0,
            mode: Mode::Browse,
            route_editor: None,
            hits: vec![],
            account_scroll: 0,
            route_scroll: 0,
        }
    }
    pub fn update_models(
        &mut self,
        models: impl IntoIterator<Item = agent_client_protocol::ModelInfo>,
    ) {
        self.models = account_models(models);
    }
    pub fn dirty(&self) -> bool {
        self.routing != self.saved
    }
    pub fn begin(&mut self, request: AccountsRequest) -> Option<(u64, CancellationToken)> {
        if self.busy.is_some() || self.auto_activation_pending {
            return None;
        }
        if let AccountsRequest::Save(routing) = &request {
            if let Err(error) = self.validate_routing(routing) {
                self.message = Some(error);
                return None;
            }
        }
        self.request_seq += 1;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.login_feedback = None;
        self.message = Some(request.message().into());
        self.busy = Some(request);
        Some((self.request_seq, cancel))
    }
    pub fn complete(&mut self, seq: u64, result: Result<AccountsData, String>) -> Option<String> {
        if self.request_seq != seq || self.busy.is_none() {
            return None;
        }
        let request = self.busy.take().unwrap();
        self.cancel = None;
        self.login_feedback = None;
        match result {
            Ok(data) => {
                self.accounts = data.accounts;
                if let Some(catalog) = data.catalog {
                    self.update_models(catalog.available_models);
                }
                self.account_selected = self
                    .account_selected
                    .min(self.accounts.len().saturating_sub(1));
                if matches!(request, AccountsRequest::Save(_))
                    || matches!(request, AccountsRequest::Load) && !self.loaded
                {
                    self.saved = data.routing.clone();
                    self.routing = data.routing;
                }
                self.loaded = true;
                self.message = Some(
                    match request {
                        AccountsRequest::Save(_) => "Routing saved. Current model kept.",
                        AccountsRequest::Login(_) => "Account connected.",
                        AccountsRequest::Logout(_) => "Account disconnected.",
                        AccountsRequest::Remove(_) => "Account removed.",
                        AccountsRequest::Refresh(_) => "Quota refreshed.",
                        AccountsRequest::Add { .. } => "Account added. Opening sign-in…",
                        AccountsRequest::Rename { .. } => "Account renamed.",
                        AccountsRequest::Load => {
                            "Account changes save immediately; routing changes need Save."
                        }
                    }
                    .into(),
                );
                if let AccountsRequest::Add { id, .. } = request {
                    self.account_selected = self
                        .accounts
                        .iter()
                        .position(|a| a.account.id == id)
                        .unwrap_or(0);
                    return Some(id);
                }
            }
            Err(error) => {
                self.message = Some(error);
            }
        }
        None
    }
    pub fn validate_routing(&self, routing: &AutoRouting) -> Result<(), String> {
        // Turning off the unchanged saved profile must work even if its catalog entries vanished.
        if self.saved.enabled
            && !routing.enabled
            && routing.strategy == self.saved.strategy
            && routing.options == self.saved.options
        {
            return Ok(());
        }
        if routing.enabled && routing.options.is_empty() {
            return Err("Add a route option before enabling Auto Routing.".into());
        }
        for (i, option) in routing.options.iter().enumerate() {
            self.validate_option(option, i)?;
        }
        Ok(())
    }
    fn validate_option(&self, option: &AutoOption, index: usize) -> Result<(), String> {
        let prefix = format!("Option {}", index + 1);
        let account = self
            .accounts
            .iter()
            .find(|account| account.account.id == option.account_id)
            .ok_or_else(|| {
                format!("{prefix}: account missing. Edit this option and select an account.")
            })?;
        let provider = provider_name(account.account.provider);
        for (role, id) in [
            ("Main", Some(option.main_model.as_str())),
            ("Worker", option.worker_model.as_deref()),
            ("Worker fallback", option.worker_fallback_model.as_deref()),
        ] {
            let Some(id) = id else { continue };
            if id.is_empty() || id == "auto" {
                return Err(format!("{prefix}: choose a {role} model from the catalog."));
            }
            let model = self
                .models
                .iter()
                .find(|model| model.id == id)
                .ok_or_else(|| format!("{prefix}: {role} model is no longer in the catalog."))?;
            if role != "Main" {
                continue;
            }
            let actual = model.provider.ok_or_else(|| {
                format!(
                    "{prefix}: {role} provider is unverified. Choose a verified {provider} model."
                )
            })?;
            if actual != account.account.provider {
                return Err(format!(
                    "{prefix}: {role} must use {provider}; chosen model is {}.",
                    provider_name(actual)
                ));
            }
        }
        Ok(())
    }
    fn selected_id(&self) -> Option<String> {
        self.accounts
            .get(self.account_selected)
            .map(|a| a.account.id.clone())
    }
    fn close(&mut self) -> AccountsOutcome {
        if self.auto_activation_pending {
            self.message = Some("Routing saved. Waiting for Auto activation…".into());
            return AccountsOutcome::Changed;
        }
        if matches!(self.busy, Some(AccountsRequest::Login(_))) {
            if let Some(cancel) = self.cancel.take() {
                cancel.cancel();
            }
            self.request_seq += 1;
            self.busy = None;
            self.message = Some("Sign-in cancelled. Reloading account status…".into());
            return AccountsOutcome::Request(AccountsRequest::Load);
        }
        if matches!(
            self.busy,
            Some(
                AccountsRequest::Save(_)
                    | AccountsRequest::Add { .. }
                    | AccountsRequest::Rename { .. }
                    | AccountsRequest::Logout(_)
                    | AccountsRequest::Remove(_)
            )
        ) {
            self.message = Some("Finishing the account operation…".into());
            return AccountsOutcome::Changed;
        }
        if !matches!(self.mode, Mode::Browse) {
            if matches!(self.mode, Mode::Pick { .. }) {
                self.mode = self.route_editor.take().map(|m| *m).unwrap_or(Mode::Browse);
            } else {
                self.mode = Mode::Browse;
                self.route_editor = None;
            }
            self.message = None;
            return AccountsOutcome::Changed;
        }
        if self.dirty() {
            self.mode = Mode::Discard;
            AccountsOutcome::Changed
        } else {
            AccountsOutcome::Close
        }
    }
    pub fn handle_key(&mut self, key: &KeyEvent) -> AccountsOutcome {
        if key.kind == KeyEventKind::Release {
            return AccountsOutcome::Changed;
        }
        if key.code == KeyCode::Esc {
            return self.close();
        }
        if matches!(self.mode, Mode::Discard) {
            return if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                AccountsOutcome::Close
            } else {
                self.mode = Mode::Browse;
                AccountsOutcome::Changed
            };
        }
        if self.busy.is_some() || self.auto_activation_pending {
            if key.code == KeyCode::Char('o') {
                return self.activate(Control::OpenUrl);
            }
            if key.code == KeyCode::Char('c') {
                return self.activate(Control::CopyUrl);
            }
            if matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Char('j' | 'k')
            ) {
                self.navigate(key.code);
            }
            return AccountsOutcome::Changed;
        }
        if let Mode::Details { scroll } = &mut self.mode {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                _ => {}
            }
            return AccountsOutcome::Changed;
        }
        if matches!(self.mode, Mode::Actions) {
            self.mode = Mode::Browse;
        }
        if let Mode::Confirm { .. } = self.mode {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('y')) {
                let Mode::Confirm { request, .. } = std::mem::replace(&mut self.mode, Mode::Browse)
                else {
                    unreachable!()
                };
                return AccountsOutcome::Request(request);
            }
            if key.code == KeyCode::Char('n') {
                self.mode = Mode::Browse;
            }
            return AccountsOutcome::Changed;
        }
        if let Mode::Pick {
            query,
            selected,
            choices,
            ..
        } = &mut self.mode
        {
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
            match key.code {
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(filtered.len().saturating_sub(1)),
                KeyCode::Enter => {
                    if let Some(i) = filtered.get(*selected) {
                        return self.activate(Control::Choose(*i));
                    }
                }
                _ => {
                    query.handle_key(key);
                    *selected = 0;
                }
            }
            return AccountsOutcome::Changed;
        }
        if let Mode::Add {
            provider,
            label,
            field,
        } = &mut self.mode
        {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => *field = 1 - *field,
                KeyCode::Up | KeyCode::Left if *field == 0 => *provider = (*provider + 3) % 4,
                KeyCode::Down | KeyCode::Right | KeyCode::Char(' ') if *field == 0 => {
                    *provider = (*provider + 1) % 4
                }
                KeyCode::Enter => return self.activate(Control::Accept),
                _ if *field == 1 => {
                    label.handle_key(key);
                }
                _ => {}
            }
            return AccountsOutcome::Changed;
        }
        if let Mode::Rename { label, .. } = &mut self.mode {
            if key.code == KeyCode::Enter {
                return self.activate(Control::Accept);
            }
            label.handle_key(key);
            return AccountsOutcome::Changed;
        }
        if let Mode::Route { field, .. } = &mut self.mode {
            match key.code {
                KeyCode::Tab | KeyCode::Down => *field = (*field + 1) % 5,
                KeyCode::BackTab | KeyCode::Up => *field = (*field + 4) % 5,
                KeyCode::Enter | KeyCode::Char(' ') => return self.activate(Control::Accept),
                _ => {}
            }
            return AccountsOutcome::Changed;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.activate(Control::Save);
        }
        let control = match key.code {
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('j' | 'k') => {
                self.navigate(key.code);
                return AccountsOutcome::Changed;
            }
            KeyCode::Char('m') => Control::More,
            KeyCode::Char('i') => Control::Details,
            KeyCode::Char('a') => Control::Add,
            KeyCode::Char('e') | KeyCode::Enter => {
                if self.pane == Pane::Accounts {
                    Control::Login
                } else {
                    Control::Edit
                }
            }
            KeyCode::Char('n') => Control::Rename,
            KeyCode::Char('l') => Control::Login,
            KeyCode::Char('d') => {
                if self.pane == Pane::Accounts {
                    Control::Logout
                } else {
                    Control::Remove
                }
            }
            KeyCode::Delete | KeyCode::Char('x') => Control::Remove,
            KeyCode::Char('r') => Control::Refresh,
            KeyCode::Char(' ') => Control::Toggle,
            KeyCode::Char('t') => Control::Strategy,
            KeyCode::Char('K') => Control::Up,
            KeyCode::Char('J') => Control::Down,
            KeyCode::Char('s') => Control::Save,
            _ => return AccountsOutcome::Changed,
        };
        self.activate(control)
    }
    fn navigate(&mut self, key: KeyCode) {
        if matches!(key, KeyCode::Tab | KeyCode::BackTab) {
            self.pane = if self.pane == Pane::Accounts {
                Pane::Routing
            } else {
                Pane::Accounts
            };
            return;
        }
        let (selected, len) = if self.pane == Pane::Accounts {
            (&mut self.account_selected, self.accounts.len())
        } else {
            (&mut self.route_selected, self.routing.options.len())
        };
        if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
            *selected = selected.saturating_sub(1);
        } else {
            *selected = (*selected + 1).min(len.saturating_sub(1));
        }
    }
    pub fn paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Add {
                label, field: 1, ..
            }
            | Mode::Rename { label, .. } => {
                label.insert_paste(text);
            }
            Mode::Pick {
                query, selected, ..
            } => {
                query.insert_paste(text);
                *selected = 0;
            }
            _ => {}
        }
    }
    pub fn handle_mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) -> AccountsOutcome {
        match mw::handle_modal_mouse(&mut self.window, kind, x, y) {
            mw::ModalWindowOutcome::CloseRequested => return self.close(),
            mw::ModalWindowOutcome::Handled => return AccountsOutcome::Changed,
            _ => {}
        }
        if matches!(kind, MouseEventKind::ScrollDown | MouseEventKind::ScrollUp) {
            if let Some((_, Control::Pane(pane))) = self.hits.iter().find(|(rect, control)| {
                matches!(control, Control::Pane(_)) && rect.contains((x, y).into())
            }) {
                self.pane = *pane;
            }
            let code = if kind == MouseEventKind::ScrollDown {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            return self.handle_key(&KeyEvent::new(code, KeyModifiers::NONE));
        }
        if kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, Control::Pane(pane))) = self.hits.iter().find(|(rect, control)| {
                matches!(control, Control::Pane(_)) && rect.contains((x, y).into())
            }) {
                self.pane = *pane;
            }
            if let Some((_, control)) = self
                .hits
                .iter()
                .rev()
                .find(|(r, _)| r.contains((x, y).into()))
            {
                return self.activate(*control);
            }
        }
        AccountsOutcome::Changed
    }
    pub(super) fn activate(&mut self, control: Control) -> AccountsOutcome {
        if matches!(control, Control::OpenUrl | Control::CopyUrl) {
            if let Some(feedback) = &self.login_feedback {
                return if matches!(control, Control::OpenUrl) {
                    AccountsOutcome::OpenUrl(feedback.authorization_url.clone())
                } else {
                    AccountsOutcome::CopyUrl(feedback.authorization_url.clone())
                };
            }
            return AccountsOutcome::Changed;
        }
        if matches!(control, Control::Cancel) {
            return self.close();
        }
        if matches!(self.mode, Mode::Actions) {
            self.mode = Mode::Browse;
        }
        if let Control::Account(i) = control {
            self.pane = Pane::Accounts;
            self.account_selected = i;
            return AccountsOutcome::Changed;
        }
        if let Control::Route(i) = control {
            self.pane = Pane::Routing;
            self.route_selected = i;
            return AccountsOutcome::Changed;
        }
        if let Control::Pane(pane) = control {
            self.pane = pane;
            return AccountsOutcome::Changed;
        }
        if self.busy.is_some() || self.auto_activation_pending {
            return AccountsOutcome::Changed;
        }
        if !self.loaded && !matches!(control, Control::Refresh) {
            return AccountsOutcome::Changed;
        }
        if let Control::Choose(i) = control {
            let Mode::Pick { kind, choices, .. } = &self.mode else {
                return AccountsOutcome::Changed;
            };
            let Some((_, value)) = choices.get(i) else {
                return AccountsOutcome::Changed;
            };
            let value = value.clone();
            let kind = *kind;
            self.mode = self.route_editor.take().map(|m| *m).unwrap_or(Mode::Browse);
            let provider = self
                .accounts
                .iter()
                .find(|account| account.account.id == value)
                .map(|account| account.account.provider);
            if let Mode::Route { option, .. } = &mut self.mode {
                match kind {
                    PickKind::Account => {
                        let changed = option.account_id != value;
                        option.account_id = value;
                        if changed {
                            if !self.models.iter().any(|model| {
                                model.id == option.main_model
                                    && model.provider.is_some()
                                    && model.provider == provider
                            }) {
                                option.main_model.clear();
                            }
                            self.message = Some(format!(
                                "Account changed. Choose a verified {} Main model.",
                                provider.map(provider_name).unwrap_or("provider")
                            ));
                        }
                    }
                    PickKind::Main => option.main_model = value,
                    PickKind::Worker => option.worker_model = (!value.is_empty()).then_some(value),
                    PickKind::WorkerFallback => {
                        option.worker_fallback_model = (!value.is_empty()).then_some(value)
                    }
                }
            }
            return AccountsOutcome::Changed;
        }
        if let Control::Field(i) = control {
            match &mut self.mode {
                Mode::Add {
                    field, provider, ..
                } => {
                    if *field == 0 && i == 0 {
                        *provider = (*provider + 1) % 4;
                    }
                    *field = i;
                }
                Mode::Route { field, .. } => {
                    *field = i;
                    return self.activate(Control::Accept);
                }
                _ => {}
            }
            return AccountsOutcome::Changed;
        }
        if matches!(control, Control::Accept) {
            match &self.mode {
                Mode::Confirm { request, .. } => {
                    let request = request.clone();
                    self.mode = Mode::Browse;
                    return AccountsOutcome::Request(request);
                }
                Mode::Discard => return AccountsOutcome::Close,
                Mode::Add {
                    provider, label, ..
                } => {
                    let label = label.text().trim().to_owned();
                    if label.is_empty() {
                        self.message = Some("Enter a label for this account.".into());
                        return AccountsOutcome::Changed;
                    }
                    let provider = PROVIDERS[*provider];
                    self.mode = Mode::Browse;
                    return AccountsOutcome::Request(AccountsRequest::Add {
                        id: new_id("account"),
                        provider,
                        label,
                    });
                }
                Mode::Rename { id, label } => {
                    let label = label.text().trim().to_owned();
                    if label.is_empty() {
                        self.message = Some("Enter a label for this account.".into());
                        return AccountsOutcome::Changed;
                    }
                    let id = id.clone();
                    self.mode = Mode::Browse;
                    return AccountsOutcome::Request(AccountsRequest::Rename { id, label });
                }
                Mode::Route { field, .. } if *field < 4 => {
                    let kind = match field {
                        0 => PickKind::Account,
                        1 => PickKind::Main,
                        2 => PickKind::Worker,
                        _ => PickKind::WorkerFallback,
                    };
                    self.open_picker(kind);
                }
                Mode::Route {
                    option, original, ..
                } => {
                    if let Err(error) =
                        self.validate_option(option, original.unwrap_or(self.routing.options.len()))
                    {
                        self.message = Some(error);
                        return AccountsOutcome::Changed;
                    }
                    let option = option.clone();
                    let original = *original;
                    if let Some(i) = original {
                        self.routing.options[i] = option;
                    } else {
                        self.routing.options.push(option);
                        self.route_selected = self.routing.options.len() - 1;
                    }
                    self.mode = Mode::Browse;
                    self.message = Some("Option updated in draft. Save to apply.".into());
                }
                _ => {}
            }
            return AccountsOutcome::Changed;
        }
        if !matches!(self.mode, Mode::Browse) {
            return AccountsOutcome::Changed;
        }
        self.message = None;
        match control {
            Control::More => self.mode = Mode::Actions,
            Control::Details => self.mode = Mode::Details { scroll: 0 },
            Control::Add if self.pane == Pane::Accounts => {
                self.mode = Mode::Add {
                    provider: 0,
                    label: LineEditor::default(),
                    field: 0,
                }
            }
            Control::Add | Control::Edit if self.pane == Pane::Routing => {
                if self.accounts.is_empty() {
                    self.message = Some("Add an account first.".into());
                } else {
                    let original = matches!(control, Control::Edit)
                        .then_some(self.route_selected)
                        .filter(|i| *i < self.routing.options.len());
                    let option =
                        original
                            .map(|i| self.routing.options[i].clone())
                            .unwrap_or(AutoOption {
                                id: new_id("route"),
                                account_id: self.accounts[self.account_selected].account.id.clone(),
                                main_model: String::new(),
                                worker_fallback_model: None,
                                worker_model: None,
                            });
                    self.mode = Mode::Route {
                        original,
                        option,
                        field: 0,
                    };
                }
            }
            Control::Rename => {
                if let Some(a) = self.accounts.get(self.account_selected) {
                    let mut label = LineEditor::default();
                    label.set_text(&a.account.label);
                    self.mode = Mode::Rename {
                        id: a.account.id.clone(),
                        label,
                    };
                }
            }
            Control::Login => {
                if let Some(id) = self.selected_id() {
                    return AccountsOutcome::Request(AccountsRequest::Login(id));
                }
            }
            Control::Logout => {
                if let Some(id) = self.selected_id() {
                    let label = &self.accounts[self.account_selected].account.label;
                    self.mode = Mode::Confirm {
                        prompt: format!("Disconnect {label}? Other accounts stay connected."),
                        request: AccountsRequest::Logout(id),
                    };
                }
            }
            Control::Remove if self.pane == Pane::Routing => {
                if self.route_selected < self.routing.options.len() {
                    self.routing.options.remove(self.route_selected);
                    self.route_selected = self
                        .route_selected
                        .min(self.routing.options.len().saturating_sub(1));
                }
            }
            Control::Remove => {
                if let Some(a) = self.accounts.get(self.account_selected) {
                    if a.account.legacy {
                        self.message = Some(
                            "Existing default accounts can be disconnected or renamed.".into(),
                        );
                    } else if self
                        .routing
                        .options
                        .iter()
                        .chain(self.saved.options.iter())
                        .any(|o| o.account_id == a.account.id)
                    {
                        self.message = Some(
                            "Remove this account's route options and Save before removing it."
                                .into(),
                        );
                    } else {
                        self.mode = Mode::Confirm {
                            prompt: format!("Remove {} and its saved connection?", a.account.label),
                            request: AccountsRequest::Remove(a.account.id.clone()),
                        };
                    }
                }
            }
            Control::Refresh => {
                return AccountsOutcome::Request(if self.loaded {
                    match self.selected_id() {
                        Some(id) => AccountsRequest::Refresh(id),
                        None => AccountsRequest::Load,
                    }
                } else {
                    AccountsRequest::Load
                });
            }
            Control::Toggle => self.routing.enabled = !self.routing.enabled,
            Control::Strategy => {
                self.routing.strategy = if self.routing.strategy == AutoStrategy::OrderedFirst {
                    AutoStrategy::HighestRemaining
                } else {
                    AutoStrategy::OrderedFirst
                }
            }
            Control::Up | Control::Down => {
                let next = if matches!(control, Control::Up) {
                    self.route_selected.saturating_sub(1)
                } else {
                    self.route_selected + 1
                };
                if self.route_selected < self.routing.options.len()
                    && next < self.routing.options.len()
                {
                    self.routing.options.swap(self.route_selected, next);
                    self.route_selected = next;
                }
            }
            Control::Save => {
                if let Err(error) = self.validate_routing(&self.routing) {
                    self.message = Some(error);
                } else {
                    return AccountsOutcome::Request(AccountsRequest::Save(self.routing.clone()));
                }
            }
            _ => {}
        }
        AccountsOutcome::Changed
    }
    fn open_picker(&mut self, kind: PickKind) {
        let mut choices: Vec<(String, String)> = match kind {
            PickKind::Account => self
                .accounts
                .iter()
                .map(|a| {
                    (
                        format!(
                            "{} · {}{}",
                            provider_name(a.account.provider),
                            a.account.label,
                            if a.connected { "" } else { " (disconnected)" }
                        ),
                        a.account.id.clone(),
                    )
                })
                .collect(),
            PickKind::Main | PickKind::Worker | PickKind::WorkerFallback => {
                let provider = match &self.mode {
                    Mode::Route { option, .. } => self
                        .accounts
                        .iter()
                        .find(|account| account.account.id == option.account_id)
                        .map(|account| account.account.provider),
                    _ => None,
                };
                self.models
                    .iter()
                    .filter(|model| {
                        !matches!(kind, PickKind::Main)
                            || (model.provider.is_some() && model.provider == provider)
                    })
                    .map(|model| (model.name.clone(), model.id.clone()))
                    .collect()
            }
        };
        if matches!(kind, PickKind::Worker) {
            choices.insert(0, ("Same as Main".into(), String::new()));
        } else if matches!(kind, PickKind::WorkerFallback) {
            choices.insert(0, ("None".into(), String::new()));
        }
        if choices.is_empty() {
            let provider = match &self.mode {
                Mode::Route { option, .. } => self
                    .accounts
                    .iter()
                    .find(|account| account.account.id == option.account_id)
                    .map(|account| provider_name(account.account.provider))
                    .unwrap_or("selected provider"),
                _ => "selected provider",
            };
            self.message = Some(format!(
                "No verified {provider} models available. Connect that provider and refresh the model catalog."
            ));
            return;
        }
        let current = match &self.mode {
            Mode::Route { option, .. } => match kind {
                PickKind::Account => option.account_id.as_str(),
                PickKind::Main => option.main_model.as_str(),
                PickKind::Worker => option.worker_model.as_deref().unwrap_or(""),
                PickKind::WorkerFallback => option.worker_fallback_model.as_deref().unwrap_or(""),
            },
            _ => "",
        };
        let selected = choices
            .iter()
            .position(|(_, id)| id == current)
            .unwrap_or(0);
        let editor = std::mem::replace(
            &mut self.mode,
            Mode::Pick {
                kind,
                choices,
                query: LineEditor::default(),
                selected,
            },
        );
        self.route_editor = Some(Box::new(editor));
    }
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}-{:016x}", rand::random::<u64>())
}
