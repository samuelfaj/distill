//! Credit balance indicator for the agent status bar.
//!
//! Shows the user's coding credit usage as a compact status bar item.
//! Fetches real data from the `x.ai/billing` agent extension.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Credit balance state from the billing API.
#[derive(Debug, Clone)]
pub struct CreditBalance {
    /// Usage as a percentage of the allowance (0.0 to 100.0).
    pub usage_pct: f64,
    /// Usage as a percentage of total budget (free and on-demand when enabled).
    pub effective_usage_pct: f64,
    /// Billing period end as a formatted local wall-clock string (no zone label), e.g. "Mar 31, 12:00".
    pub period_end_display: Option<String>,
    /// Whether pay-as-you-go (on-demand) billing is enabled.
    pub pay_as_you_go: bool,
    /// On-demand spending cap in USD cents (e.g. 500 is $5.00).
    pub on_demand_cap_cents: Option<i64>,
    /// On-demand usage this period in USD cents.
    pub on_demand_used_cents: Option<i64>,
    /// Remaining prepaid ("bought") credit balance in USD cents.
    pub prepaid_balance_cents: Option<i64>,
    /// Usage period type from the billing response (the proto enum name, e.g. `USAGE_PERIOD_TYPE_WEEKLY`).
    /// Drives the "Weekly/Monthly limit" label.
    pub period_type: Option<String>,
    /// From credits config `is_unified_billing_user` (`None` if absent).
    /// `Some(true)` means the unified pool and buy-credits UX; `Some(false)` means the legacy on-demand (PAYG) UX.
    pub is_unified_billing_user: Option<bool>,
}

impl CreditBalance {
    /// Label for the percentage allowance, chosen from the period type: "Weekly limit" / "Monthly limit", falling back to "Usage" when unknown.
    pub fn usage_label(&self) -> &'static str {
        match self.period_type.as_deref() {
            Some(t) if t.contains("WEEKLY") => "Weekly limit",
            Some(t) if t.contains("MONTHLY") => "Monthly limit",
            _ => "Usage",
        }
    }
}

/// Auto top-up rule data used by the `/usage` summary.
#[derive(Debug, Clone)]
pub struct AutoTopupInfo {
    /// Whether auto top-up is enabled.
    pub enabled: bool,
    /// Per-trigger top-up amount in USD cents.
    pub topup_amount_cents: Option<i64>,
    /// Optional maximum monthly top-up amount in USD cents.
    pub max_amount_cents: Option<i64>,
}

impl AutoTopupInfo {
    /// A known "auto top-up disabled" state, distinct from an unresolved `None`, which means the rule hasn't been fetched yet.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            topup_amount_cents: None,
            max_amount_cents: None,
        }
    }
}

/// Outcome of an auto top-up rule fetch, so a transient failure doesn't clear a previously cached rule.
#[derive(Debug, Clone)]
pub enum AutoTopupFetch {
    /// A definitive rule state (a real rule, or [`AutoTopupInfo::disabled`] when the backend reports none).
    /// Stored as the *known* auto top-up state.
    Resolved(AutoTopupInfo),
    /// Fetch failed; keep the cached value.
    /// A stored `None` therefore means "not yet known", not "no auto top-up".
    Unchanged,
    /// The rule is not applicable (no prepaid credits); reset the cache to "unknown" so a later credits period doesn't read a stale rule.
    Cleared,
}

/// Format `cents` as a dollar string: whole dollars as `$N`, otherwise `$N.NN`.
fn fmt_dollars(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    if dollars.fract() == 0.0 {
        format!("${dollars:.0}")
    } else {
        format!("${dollars:.2}")
    }
}

/// Build the `/usage` summary block shown in scrollback. Always shows usage % and (when known) the
/// next reset time. The credits block is rendered only when the user has a positive prepaid
/// balance.
pub fn format_usage_summary(balance: &CreditBalance, autotopup: Option<&AutoTopupInfo>) -> String {
    // Floor to match the backend SpendingLimiter's `as u8` truncation (99.994% renders as 99%, never 100% until truly exhausted)
    let mut lines = vec![format!(
        "{}: {}%",
        balance.usage_label(),
        balance.usage_pct.floor() as i64
    )];
    if let Some(reset) = &balance.period_end_display {
        lines.push(format!("Next reset: {reset}"));
    }

    // Billing stores credit / top-up amounts as negative cents (accounting convention); display the absolute USD value, matching the web clients
    if let Some(prepaid) = balance
        .prepaid_balance_cents
        .map(i64::abs)
        .filter(|c| *c > 0)
    {
        lines.push(String::new());
        lines.push(format!("Credits: {}", fmt_dollars(prepaid)));
        match autotopup {
            Some(at) if at.enabled && at.topup_amount_cents.is_some() => {
                lines.push(format!(
                    "Auto topup: {}",
                    fmt_dollars(at.topup_amount_cents.unwrap().abs())
                ));
                if let Some(max) = at.max_amount_cents {
                    lines.push(format!("Max monthly topup: {}", fmt_dollars(max.abs())));
                }
            }
            _ => lines.push("Auto topup: disabled".to_string()),
        }
    }

    // Legacy on-demand (pay-as-you-go) billing, shown only when enabled, for users on the older monthly and on-demand model
    // Amounts always carry cents (e.g. `$50.00`), matching the web client.
    if balance.pay_as_you_go {
        let used = balance.on_demand_used_cents.unwrap_or(0).abs() as f64 / 100.0;
        let cap = balance.on_demand_cap_cents.unwrap_or(0).abs() as f64 / 100.0;
        lines.push(String::new());
        lines.push(format!("Pay-as-you-go: ${used:.2} used of ${cap:.2} limit"));
    }

    lines.join("\n")
}

/// Provider of the effective main model, if its catalog metadata identifies one.
pub fn main_provider(models: &crate::acp::ModelState) -> Option<distill_shell::accounts::Provider> {
    let id = models
        .auto_route
        .as_ref()
        .map(|route| &route.0)
        .or(models.current.as_ref())?;
    models
        .available
        .get(id)?
        .meta
        .as_ref()?
        .get(distill_shell::agent::config::ACCOUNT_PROVIDER_META_KEY)?
        .as_str()?
        .parse()
        .ok()
}

/// Select the warning for the effective main model's provider.
pub fn provider_usage_warning(
    models: Option<&crate::acp::ModelState>,
    balance: Option<&CreditBalance>,
    autotopup: Option<&AutoTopupInfo>,
    usage_visible: bool,
    gateway_chat: bool,
    chatgpt_usage: Option<&distill_shell::codex_auth::CodexUsageSnapshot>,
) -> Option<(String, bool)> {
    let models = models?;
    match main_provider(models)? {
        distill_shell::accounts::Provider::Grok => {
            usage_warning_for_session(balance?, autotopup, usage_visible, gateway_chat)
        }
        distill_shell::accounts::Provider::Chatgpt
            if !gateway_chat && models.auto_route.is_none() =>
        {
            chatgpt_weekly_warning(chatgpt_usage?, chrono::Utc::now())
        }
        _ => None,
    }
}

fn chatgpt_weekly_warning(
    usage: &distill_shell::codex_auth::CodexUsageSnapshot,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, bool)> {
    let age = now.signed_duration_since(usage.fetched_at);
    if age < chrono::Duration::zero() || age > chrono::Duration::minutes(5) {
        return None;
    }
    let limits = usage.rate_limit.as_ref()?;
    let window = [
        limits.primary_window.as_ref(),
        limits.secondary_window.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find(|window| window.limit_window_seconds == 604800)?;
    let used = window.used_percent;
    if !used.is_finite()
        || !(0.0..=100.0).contains(&used)
        || (window.reset_at > 0 && window.reset_at <= now.timestamp())
        || (window.reset_at <= 0 && window.reset_after_seconds <= age.num_seconds())
        || used <= 90.0
    {
        return None;
    }
    Some((
        format!(
            "ChatGPT weekly limit left: {}%",
            100 - used.floor() as i64
        ),
        used > 95.0,
    ))
}

/// Low-balance ($10) and pay-as-you-go critical ($5) warning thresholds, in cents.
const LOW_BALANCE_CENTS: i64 = 1000;
const PAY_AS_YOU_GO_CRITICAL_CENTS: i64 = 500;

/// The prompt's usage/credits warning as `(text, critical)`, or `None`. `critical` renders yellow,
/// else grey; team users with `usage_visible = false` never warn. Gateway light-frontend (`kind:
/// "chat"`) sessions must not show Build coding-credit warnings.
pub fn usage_warning(
    balance: &CreditBalance,
    autotopup: Option<&AutoTopupInfo>,
    usage_visible: bool,
) -> Option<(String, bool)> {
    usage_warning_for_session(balance, autotopup, usage_visible, false)
}

/// Like [`usage_warning`], but suppresses output for gateway/chat-kind sessions.
pub fn usage_warning_for_session(
    balance: &CreditBalance,
    autotopup: Option<&AutoTopupInfo>,
    usage_visible: bool,
    gateway_chat: bool,
) -> Option<(String, bool)> {
    if gateway_chat || !usage_visible {
        return None;
    }

    // A non-zero prepaid balance (stored as signed cents) means the credits model.
    let credits = balance
        .prepaid_balance_cents
        .map(i64::abs)
        .filter(|c| *c > 0);

    let Some(credits_cents) = credits else {
        // Pay-as-you-go (legacy on-demand): warn on dollars left in the cap once the included allowance is spent
        if balance.pay_as_you_go {
            if balance.usage_pct >= 100.0 {
                let cap = balance.on_demand_cap_cents.unwrap_or(0).abs();
                let used = balance.on_demand_used_cents.unwrap_or(0).abs();
                let remaining = (cap - used).max(0);
                if remaining <= LOW_BALANCE_CENTS {
                    let text = format!("Pay-as-you-go limit left: {}", fmt_dollars(remaining));
                    return Some((text, remaining <= PAY_AS_YOU_GO_CRITICAL_CENTS));
                }
            }
            return None;
        }

        let pct = balance.effective_usage_pct;
        if pct > 90.0 {
            // "Left" is the complement of floored usage, so it agrees with the floored summary: 99.994% shows "1% left", not "0%"
            let remaining = (100 - pct.floor() as i64).max(0);
            let label = balance.usage_label();
            return Some((format!("{label} left: {remaining}%"), pct > 95.0));
        }
        return None;
    };

    // Credits are only drawn down at 100% usage; don't warn before then.
    if balance.usage_pct < 100.0 {
        return None;
    }

    let credits_warning = || {
        (
            format!("Credits left: {}", fmt_dollars(credits_cents)),
            true,
        )
    };

    // Auto top-up gates the warning: unknown stays silent; disabled warns when low
    // Enabled without a max never warns; enabled with a max warns below one top-up amount
    match autotopup {
        None => None,
        Some(at) if !at.enabled => (credits_cents <= LOW_BALANCE_CENTS).then(credits_warning),
        Some(at) if at.max_amount_cents.is_none() => None,
        Some(at) => at
            .topup_amount_cents
            .map(i64::abs)
            .and_then(|amt| (credits_cents < amt).then(credits_warning)),
    }
}

/// Gateway light-frontend (`kind: "chat"`) sessions must not show Build coding credits. Remote
/// settings or a managed opt-in for chat entry can share the same gate later; for now it only
/// suppresses misleading local telemetry.
pub fn credit_bar_line(balance: &CreditBalance, hovered: bool, theme: &Theme) -> Line<'static> {
    credit_bar_line_for_session(balance, hovered, theme, false)
        .expect("non-chat credit_bar_line always renders")
}

/// Like [`credit_bar_line`], but returns `None` for gateway/chat-kind sessions so the status bar never implies Build sampler / coding-credit usage.
pub fn credit_bar_line_for_session(
    balance: &CreditBalance,
    _hovered: bool,
    theme: &Theme,
    gateway_chat: bool,
) -> Option<Line<'static>> {
    if gateway_chat {
        return None;
    }
    let pct = balance.usage_pct;
    let color = if pct >= 100.0 {
        theme.accent_error
    } else if pct >= 80.0 {
        theme.warning
    } else {
        theme.accent_success
    };

    let text = format!("Credits used: {pct:.0}%");

    let style = Style::default().fg(color).bg(theme.bg_base);
    Some(Line::from(Span::styled(text, style)))
}

#[cfg(test)]
pub(crate) mod weekly_test_fixtures {
    use super::*;
    pub fn snapshot() -> distill_shell::codex_auth::CodexUsageSnapshot {
        let mut snapshot: distill_shell::codex_auth::CodexUsageSnapshot = serde_json::from_value(serde_json::json!({"rate_limit": {"primary_window": {"used_percent": 96.5, "limit_window_seconds": 604800, "reset_after_seconds": 3600, "reset_at": 0}}})).unwrap();
        snapshot.fetched_at = chrono::Utc::now();
        snapshot
    }
    pub fn models() -> crate::acp::ModelState {
        use agent_client_protocol::{ModelId, ModelInfo};
        let mut models = crate::acp::ModelState::default();
        for (id, provider) in [("gpt", "chatgpt"), ("grok", "grok"), ("other", "claude")] {
            let id = ModelId::new(id);
            models.available.insert(
                id.clone(),
                ModelInfo::new(id.clone(), id.0.to_string()).meta(
                    serde_json::json!({"accountProvider": provider})
                        .as_object()
                        .cloned(),
                ),
            );
        }
        models.current = Some(ModelId::new("gpt"));
        models
    }
    pub fn balance() -> CreditBalance {
        CreditBalance {
            usage_pct: 92.0,
            effective_usage_pct: 92.0,
            period_end_display: None,
            pay_as_you_go: false,
            on_demand_cap_cents: None,
            on_demand_used_cents: None,
            prepaid_balance_cents: None,
            period_type: Some("WEEKLY".into()),
            is_unified_billing_user: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_weekly_validation_and_provider_selection() {
        use agent_client_protocol::ModelId;
        use weekly_test_fixtures::{balance, models, snapshot};
        let now = chrono::Utc::now();
        let mut usage = snapshot();
        usage.fetched_at = now;
        let warning =
            |u: &distill_shell::codex_auth::CodexUsageSnapshot| chatgpt_weekly_warning(u, now);
        assert_eq!(
            warning(&usage),
            Some(("ChatGPT weekly limit left: 4%".into(), true))
        );
        for (pct, expected) in [
            (90.0, None),
            (90.1, Some(("ChatGPT weekly limit left: 10%".into(), false))),
            (95.0, Some(("ChatGPT weekly limit left: 5%".into(), false))),
            (95.1, Some(("ChatGPT weekly limit left: 5%".into(), true))),
            (f64::NAN, None),
            (f64::INFINITY, None),
            (-1.0, None),
            (101.0, None),
        ] {
            usage
                .rate_limit
                .as_mut()
                .unwrap()
                .primary_window
                .as_mut()
                .unwrap()
                .used_percent = pct;
            assert_eq!(warning(&usage), expected);
        }
        usage = snapshot();
        usage.fetched_at = now;
        let window = usage.rate_limit.as_mut().unwrap().primary_window.take();
        usage.rate_limit.as_mut().unwrap().secondary_window = window;
        assert!(warning(&usage).is_some());
        usage
            .rate_limit
            .as_mut()
            .unwrap()
            .secondary_window
            .as_mut()
            .unwrap()
            .limit_window_seconds = 18000;
        assert!(warning(&usage).is_none());
        usage = snapshot();
        usage.fetched_at = now - chrono::Duration::minutes(5);
        assert!(warning(&usage).is_some());
        usage.fetched_at = now - chrono::Duration::minutes(5) - chrono::Duration::milliseconds(1);
        assert!(warning(&usage).is_none());
        usage.fetched_at = now + chrono::Duration::milliseconds(1);
        assert!(warning(&usage).is_none());
        usage.fetched_at = now;
        let w = usage
            .rate_limit
            .as_mut()
            .unwrap()
            .primary_window
            .as_mut()
            .unwrap();
        w.reset_at = now.timestamp();
        assert!(warning(&usage).is_none());
        let w = usage
            .rate_limit
            .as_mut()
            .unwrap()
            .primary_window
            .as_mut()
            .unwrap();
        w.reset_at = 0;
        w.reset_after_seconds = 0;
        assert!(warning(&usage).is_none());
        usage.fetched_at = now - chrono::Duration::seconds(60);
        usage.rate_limit.as_mut().unwrap().primary_window.as_mut().unwrap().reset_after_seconds = 60;
        assert!(warning(&usage).is_none());
        usage = snapshot();
        let mut models = models();
        let balance = balance();
        let select = |m: &crate::acp::ModelState, chat, visible, u| {
            provider_usage_warning(Some(m), Some(&balance), None, visible, chat, u)
        };
        assert!(select(&models, false, false, Some(&usage)).is_some());
        assert!(select(&models, true, true, Some(&usage)).is_none());
        assert!(select(&models, false, true, None).is_none());
        models.worker_model = Some(ModelId::new("grok"));
        assert!(
            select(&models, false, true, Some(&usage))
                .unwrap()
                .0
                .starts_with("ChatGPT")
        );
        models.auto_route = Some((ModelId::new("gpt"), ModelId::new("grok")));
        assert!(select(&models, false, true, Some(&usage)).is_none());
        models.auto_route = Some((ModelId::new("grok"), ModelId::new("gpt")));
        assert!(
            select(&models, false, true, Some(&usage))
                .unwrap()
                .0
                .starts_with("Weekly")
        );
        models.auto_route = None;
        for id in ["other", "missing"] {
            models.current = Some(ModelId::new(id));
            assert!(select(&models, false, true, Some(&usage)).is_none());
        }
    }

    #[test]
    fn main_provider_uses_metadata_and_auto_main_not_worker() {
        use agent_client_protocol::{ModelId, ModelInfo};
        use distill_shell::accounts::Provider;
        let mut models = crate::acp::ModelState::default();
        let main = ModelId::new("main");
        let worker = ModelId::new("worker");
        let info = |id: &ModelId, provider: &str| {
            ModelInfo::new(id.clone(), id.0.to_string()).meta(
                serde_json::json!({"accountProvider": provider})
                    .as_object()
                    .cloned(),
            )
        };
        models
            .available
            .insert(main.clone(), info(&main, "chatgpt"));
        models
            .available
            .insert(worker.clone(), info(&worker, "grok"));
        models.current = Some(main.clone());
        models.worker_model = Some(worker.clone());
        assert_eq!(main_provider(&models), Some(Provider::Chatgpt));
        models.auto_route = Some((worker.clone(), main.clone()));
        assert_eq!(main_provider(&models), Some(Provider::Grok));
        models
            .available
            .insert(worker.clone(), info(&worker, "unknown"));
        assert_eq!(main_provider(&models), None);
        models
            .available
            .insert(worker.clone(), ModelInfo::new(worker, "worker"));
        assert_eq!(main_provider(&models), None);
    }

    fn bal(pct: f64) -> CreditBalance {
        CreditBalance {
            usage_pct: pct,
            effective_usage_pct: pct,
            period_end_display: None,
            pay_as_you_go: false,
            on_demand_cap_cents: None,
            on_demand_used_cents: None,
            prepaid_balance_cents: None,
            period_type: None,
            is_unified_billing_user: None,
        }
    }

    fn topup(enabled: bool, amount: Option<i64>, max: Option<i64>) -> AutoTopupInfo {
        AutoTopupInfo {
            enabled,
            topup_amount_cents: amount,
            max_amount_cents: max,
        }
    }

    #[test]
    fn summary_no_credits_omits_credits_block() {
        let b = CreditBalance {
            period_end_display: Some("June 14, 16:00".into()),
            prepaid_balance_cents: Some(0),
            ..bal(25.0)
        };
        // Even with an auto-topup rule present, zero prepaid omits the credits block
        let out = format_usage_summary(&b, Some(&topup(true, Some(2000), Some(10000))));
        assert_eq!(out, "Usage: 25%\nNext reset: June 14, 16:00");
    }

    #[test]
    fn summary_credits_without_autotopup_shows_disabled() {
        let b = CreditBalance {
            prepaid_balance_cents: Some(10000),
            ..bal(25.0)
        };
        assert_eq!(
            format_usage_summary(&b, None),
            "Usage: 25%\n\nCredits: $100\nAuto topup: disabled"
        );
        // A disabled rule renders the same.
        assert_eq!(
            format_usage_summary(&b, Some(&topup(false, Some(2000), Some(10000)))),
            "Usage: 25%\n\nCredits: $100\nAuto topup: disabled"
        );
    }

    #[test]
    fn summary_autotopup_enabled_without_max_omits_max() {
        let b = CreditBalance {
            prepaid_balance_cents: Some(10000),
            ..bal(25.0)
        };
        assert_eq!(
            format_usage_summary(&b, Some(&topup(true, Some(2000), None))),
            "Usage: 25%\n\nCredits: $100\nAuto topup: $20"
        );
    }

    #[test]
    fn summary_autotopup_enabled_with_max_renders_all() {
        let b = CreditBalance {
            period_end_display: Some("June 14, 16:00".into()),
            prepaid_balance_cents: Some(10000),
            ..bal(25.0)
        };
        assert_eq!(
            format_usage_summary(&b, Some(&topup(true, Some(2000), Some(10000)))),
            "Usage: 25%\nNext reset: June 14, 16:00\n\nCredits: $100\nAuto topup: $20\nMax monthly topup: $100"
        );
    }

    #[test]
    fn summary_formats_fractional_dollars() {
        let b = CreditBalance {
            prepaid_balance_cents: Some(1250),
            ..bal(25.0)
        };
        assert_eq!(
            format_usage_summary(&b, Some(&topup(true, Some(550), None))),
            "Usage: 25%\n\nCredits: $12.50\nAuto topup: $5.50"
        );
    }

    #[test]
    fn summary_abs_negative_billing_amounts() {
        // Billing returns credit / top-up amounts as negative cents; the summary must render them as positive USD (matching the web)
        let b = CreditBalance {
            prepaid_balance_cents: Some(-500),
            ..bal(100.0)
        };
        assert_eq!(
            format_usage_summary(&b, Some(&topup(true, Some(-500), Some(-1000)))),
            "Usage: 100%\n\nCredits: $5\nAuto topup: $5\nMax monthly topup: $10"
        );
    }

    #[test]
    fn summary_pay_as_you_go_enabled_renders_used_of_limit() {
        let b = CreditBalance {
            pay_as_you_go: true,
            on_demand_used_cents: Some(355),
            on_demand_cap_cents: Some(5000),
            period_type: Some("USAGE_PERIOD_TYPE_MONTHLY".into()),
            period_end_display: Some("June 30, 16:00".into()),
            ..bal(91.0)
        };
        assert_eq!(
            format_usage_summary(&b, None),
            "Monthly limit: 91%\nNext reset: June 30, 16:00\n\nPay-as-you-go: $3.55 used of $50.00 limit"
        );
    }

    #[test]
    fn summary_pay_as_you_go_disabled_omits_line() {
        let b = CreditBalance {
            pay_as_you_go: false,
            period_type: Some("USAGE_PERIOD_TYPE_MONTHLY".into()),
            period_end_display: Some("June 30, 16:00".into()),
            ..bal(91.0)
        };
        assert_eq!(
            format_usage_summary(&b, None),
            "Monthly limit: 91%\nNext reset: June 30, 16:00"
        );
    }

    // ── usage_label / period type ────────────────────────────────────

    fn bal_period(pct: f64, period_type: &str) -> CreditBalance {
        CreditBalance {
            period_type: Some(period_type.to_string()),
            ..bal(pct)
        }
    }

    #[test]
    fn usage_label_from_period_type() {
        assert_eq!(
            bal_period(0.0, "USAGE_PERIOD_TYPE_WEEKLY").usage_label(),
            "Weekly limit"
        );
        assert_eq!(
            bal_period(0.0, "USAGE_PERIOD_TYPE_MONTHLY").usage_label(),
            "Monthly limit"
        );
        // Unknown / unspecified / absent falls back to "Usage"
        assert_eq!(
            bal_period(0.0, "USAGE_PERIOD_TYPE_UNSPECIFIED").usage_label(),
            "Usage"
        );
        assert_eq!(bal(0.0).usage_label(), "Usage");
    }

    #[test]
    fn summary_uses_period_label() {
        let weekly = bal_period(25.0, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(format_usage_summary(&weekly, None), "Weekly limit: 25%");
        let monthly = bal_period(25.0, "USAGE_PERIOD_TYPE_MONTHLY");
        assert_eq!(format_usage_summary(&monthly, None), "Monthly limit: 25%");
    }

    #[test]
    fn warning_uses_period_label() {
        let weekly = bal_period(92.0, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(
            usage_warning(&weekly, None, true),
            Some(("Weekly limit left: 8%".to_string(), false))
        );
    }

    #[test]
    fn summary_floors_usage_percent() {
        // Match the backend SpendingLimiter (`as u8` truncation): 99.994% must render as 99%, not round up to 100%
        let almost = bal_period(99.994, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(format_usage_summary(&almost, None), "Weekly limit: 99%");
        // A true 100% still shows 100%.
        let full = bal_period(100.0, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(format_usage_summary(&full, None), "Weekly limit: 100%");
    }

    #[test]
    fn warning_percent_left_is_floor_complement() {
        // 99.994% used floors to 99%, so it shows "1% left" (not "0% left"), and the warning and the floored summary always sum to 100
        let almost = bal_period(99.994, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(
            usage_warning(&almost, None, true),
            Some(("Weekly limit left: 1%".to_string(), true))
        );
        // A true 100% (no credits) shows "0% left"
        let full = bal_period(100.0, "USAGE_PERIOD_TYPE_WEEKLY");
        assert_eq!(
            usage_warning(&full, None, true),
            Some(("Weekly limit left: 0%".to_string(), true))
        );
    }

    // ── usage_warning (prompt info row) ──────────────────────────────

    #[test]
    fn warning_usage_model_thresholds() {
        assert_eq!(usage_warning(&bal(50.0), None, true), None);
        assert_eq!(
            usage_warning(&bal(92.0), None, true),
            Some(("Usage left: 8%".to_string(), false))
        );
        assert_eq!(
            usage_warning(&bal(97.0), None, true),
            Some(("Usage left: 3%".to_string(), true))
        );
    }

    #[test]
    fn warning_hidden_for_team_users() {
        assert_eq!(usage_warning(&bal(99.0), None, false), None);
        let credits = CreditBalance {
            prepaid_balance_cents: Some(100),
            ..bal(0.0)
        };
        assert_eq!(usage_warning(&credits, None, false), None);
    }

    #[test]
    fn warning_credits_unknown_topup_is_suppressed() {
        // At 100% usage with prepaid credits but the rule not yet known (None), never warn; it resolves on the next billing fetch
        let b = CreditBalance {
            prepaid_balance_cents: Some(100),
            ..bal(100.0)
        };
        assert_eq!(usage_warning(&b, None, true), None);
    }

    #[test]
    fn warning_credits_suppressed_below_full_usage() {
        // Low credits and no auto top-up, but the included allowance still has room (usage < 100%), so no warning; credits aren't being spent yet
        let disabled = topup(false, None, None);
        let low = CreditBalance {
            prepaid_balance_cents: Some(453),
            ..bal(0.0)
        };
        assert_eq!(usage_warning(&low, Some(&disabled), true), None);
        // The same balance warns once the allowance is exhausted
        let exhausted = CreditBalance {
            prepaid_balance_cents: Some(453),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&exhausted, Some(&disabled), true),
            Some(("Credits left: $4.53".to_string(), true))
        );
    }

    #[test]
    fn warning_credits_no_topup_low_shows_dollars() {
        // "No auto top-up" is a known, disabled rule (not an unresolved None).
        let b = CreditBalance {
            prepaid_balance_cents: Some(453),
            ..bal(100.0)
        };
        let disabled = topup(false, None, None);
        assert_eq!(
            usage_warning(&b, Some(&disabled), true),
            Some(("Credits left: $4.53".to_string(), true))
        );
    }

    #[test]
    fn warning_credits_no_topup_above_threshold_silent() {
        let disabled = topup(false, None, None);
        let b = CreditBalance {
            prepaid_balance_cents: Some(1500),
            ..bal(100.0)
        };
        assert_eq!(usage_warning(&b, Some(&disabled), true), None);
        // Exactly $10 is still "low".
        let at_ten = CreditBalance {
            prepaid_balance_cents: Some(1000),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&at_ten, Some(&disabled), true),
            Some(("Credits left: $10".to_string(), true))
        );
    }

    #[test]
    fn warning_credits_topup_no_max_never_warns() {
        let b = CreditBalance {
            prepaid_balance_cents: Some(1),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&b, Some(&topup(true, Some(2000), None)), true),
            None
        );
    }

    #[test]
    fn warning_credits_topup_with_max_below_topup_amount() {
        // $15 balance, $20 top-up amount, $100 max: below one top-up, so it warns
        let b = CreditBalance {
            prepaid_balance_cents: Some(1500),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&b, Some(&topup(true, Some(2000), Some(10000))), true),
            Some(("Credits left: $15".to_string(), true))
        );
        let plenty = CreditBalance {
            prepaid_balance_cents: Some(2500),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&plenty, Some(&topup(true, Some(2000), Some(10000))), true),
            None
        );
    }

    #[test]
    fn warning_credits_handles_negative_cents() {
        let b = CreditBalance {
            prepaid_balance_cents: Some(-453),
            ..bal(100.0)
        };
        assert_eq!(
            usage_warning(&b, Some(&topup(true, Some(-2000), Some(-10000))), true),
            Some(("Credits left: $4.53".to_string(), true))
        );
    }

    #[test]
    fn warning_credits_take_precedence_over_usage() {
        // A credits user below 100% usage gets no warning: no usage-% warning, and credits aren't being spent yet
        // A non-credits user would see "Usage left: 1%" at 99%
        let b = CreditBalance {
            prepaid_balance_cents: Some(5000),
            ..bal(99.0)
        };
        assert_eq!(
            usage_warning(&b, Some(&topup(false, None, None)), true),
            None
        );
        // Zero prepaid falls back to the usage model.
        let zero = CreditBalance {
            prepaid_balance_cents: Some(0),
            ..bal(99.0)
        };
        assert_eq!(
            usage_warning(&zero, None, true),
            Some(("Usage left: 1%".to_string(), true))
        );
    }

    // ── usage_warning: pay-as-you-go (monthly on-demand) ─────────────

    fn pay_as_you_go(usage_pct: f64, cap_cents: i64, used_cents: i64) -> CreditBalance {
        CreditBalance {
            pay_as_you_go: true,
            on_demand_cap_cents: Some(cap_cents),
            on_demand_used_cents: Some(used_cents),
            period_type: Some("USAGE_PERIOD_TYPE_MONTHLY".into()),
            ..bal(usage_pct)
        }
    }

    #[test]
    fn warning_pay_as_you_go_low_dollars_shows_remaining() {
        // $50 cap, $42 used leaves $8, shown grey (above $5)
        let grey = pay_as_you_go(100.0, 5000, 4200);
        assert_eq!(
            usage_warning(&grey, None, true),
            Some(("Pay-as-you-go limit left: $8".to_string(), false))
        );
        // $50 cap, $46 used leaves $4, critical (yellow)
        let yellow = pay_as_you_go(100.0, 5000, 4600);
        assert_eq!(
            usage_warning(&yellow, None, true),
            Some(("Pay-as-you-go limit left: $4".to_string(), true))
        );
    }

    #[test]
    fn warning_pay_as_you_go_boundaries() {
        // Exactly $10 left shows the warning, grey
        let at_ten = pay_as_you_go(100.0, 5000, 4000);
        assert_eq!(
            usage_warning(&at_ten, None, true),
            Some(("Pay-as-you-go limit left: $10".to_string(), false))
        );
        // Exactly $5 left is critical (yellow)
        let at_five = pay_as_you_go(100.0, 5000, 4500);
        assert_eq!(
            usage_warning(&at_five, None, true),
            Some(("Pay-as-you-go limit left: $5".to_string(), true))
        );
    }

    #[test]
    fn warning_pay_as_you_go_above_threshold_silent() {
        // $20 left (above the $10 threshold) gets no warning
        let b = pay_as_you_go(100.0, 5000, 3000);
        assert_eq!(usage_warning(&b, None, true), None);
    }

    #[test]
    fn warning_pay_as_you_go_suppressed_below_full_usage() {
        // Pay-as-you-go users get no percentage warning before the included allowance is exhausted, even with low on-demand room remaining
        let b = pay_as_you_go(95.0, 5000, 4800);
        assert_eq!(usage_warning(&b, None, true), None);
    }

    #[test]
    fn warning_pay_as_you_go_fractional_dollars() {
        // $50 cap, $46.50 used leaves $3.50: critical, fractional formatting
        let b = pay_as_you_go(100.0, 5000, 4650);
        assert_eq!(
            usage_warning(&b, None, true),
            Some(("Pay-as-you-go limit left: $3.50".to_string(), true))
        );
    }

    #[test]
    fn test_credit_bar_line_shows_percentage() {
        let theme = Theme::default();
        let line = credit_bar_line(&bal(24.0), false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Credits used: 24%");
    }

    #[test]
    fn test_color_thresholds() {
        let theme = Theme::default();

        let low = credit_bar_line(&bal(50.0), false, &theme);
        assert_eq!(
            low.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_success)
        );

        let high = credit_bar_line(&bal(85.0), false, &theme);
        assert_eq!(
            high.spans.first().and_then(|s| s.style.fg),
            Some(theme.warning)
        );

        let over = credit_bar_line(&bal(100.0), false, &theme);
        assert_eq!(
            over.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_error)
        );
    }

    #[test]
    fn test_zero_percent() {
        let theme = Theme::default();
        let line = credit_bar_line(&bal(0.0), false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Credits used: 0%");
        assert_eq!(
            line.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_success)
        );
    }

    #[test]
    fn test_boundary_at_80_percent() {
        let theme = Theme::default();
        // Exactly 80% renders yellow (warning)
        let at_80 = credit_bar_line(&bal(80.0), false, &theme);
        assert_eq!(
            at_80.spans.first().and_then(|s| s.style.fg),
            Some(theme.warning)
        );

        // Just below 80% renders green (success)
        let below_80 = credit_bar_line(&bal(79.9), false, &theme);
        assert_eq!(
            below_80.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_success)
        );
    }

    #[test]
    fn test_boundary_at_100_percent() {
        let theme = Theme::default();
        // Exactly 100% renders red (error)
        let at_100 = credit_bar_line(&bal(100.0), false, &theme);
        assert_eq!(
            at_100.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_error)
        );

        // Just below 100% renders yellow (warning)
        let below_100 = credit_bar_line(&bal(99.9), false, &theme);
        assert_eq!(
            below_100.spans.first().and_then(|s| s.style.fg),
            Some(theme.warning)
        );
    }

    #[test]
    fn test_over_100_percent() {
        let theme = Theme::default();
        let line = credit_bar_line(&bal(150.0), false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Credits used: 150%");
        assert_eq!(
            line.spans.first().and_then(|s| s.style.fg),
            Some(theme.accent_error)
        );
    }

    #[test]
    fn test_fractional_percentage_rounds_display() {
        let theme = Theme::default();
        let line = credit_bar_line(&bal(33.7), false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Credits used: 34%");
    }

    #[test]
    fn test_credit_balance_with_on_demand_fields() {
        let balance = CreditBalance {
            effective_usage_pct: 25.0,
            period_end_display: Some("Jun 1, 00:00".into()),
            pay_as_you_go: true,
            on_demand_cap_cents: Some(2000),
            on_demand_used_cents: Some(500),
            ..bal(50.0)
        };
        let theme = Theme::default();
        // The credit bar uses usage_pct (not effective_usage_pct).
        let line = credit_bar_line(&balance, false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Credits used: 50%");
    }

    #[test]
    fn gateway_chat_suppresses_credit_bar_and_usage_warning() {
        let theme = Theme::default();
        let b = bal(90.0);
        assert!(credit_bar_line_for_session(&b, false, &theme, true).is_none());
        assert!(usage_warning_for_session(&b, None, true, true).is_none());
        // Build path still renders.
        assert!(credit_bar_line_for_session(&b, false, &theme, false).is_some());
    }
}
