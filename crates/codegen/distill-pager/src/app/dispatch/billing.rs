// Modified for Distill by Samuel Fajreldines, 2026.
//! Subscription tier checks, credit-limit upsells, and auto-topup handling.

use super::queue::{maybe_drain_queue, note_peek_page_flip};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::AppView;
use crate::scrollback::block::RenderBlock;
use distill_telemetry::events::{SuperGrokUpsell, SuperGrokUpsellClicked};
use distill_telemetry::session_ctx::log_event;
use std::time::Duration;

/// How long the pager auto-checks subscription status before stopping.
/// After this, the user can still manually check via the [Refresh] button.
pub(super) const PAYWALL_AUTO_CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Whether the user is at the highest subscription tier (SuperGrok Heavy).
/// Returns `true` only when `subscription_tier` positively matches a known max-tier identifier.
/// An unknown (`None`) or unrecognized tier returns `false`, so lower-tier users always get the Q&A modal with the upgrade option.
pub(super) fn is_max_tier(subscription_tier: Option<&str>) -> bool {
    let Some(t) = subscription_tier else {
        return false; // Unknown: default to Q&A.
    };
    // Lowercase and replace spaces with underscores to match both JWT-derived keys ("supergrok_heavy") and CCP display names ("SuperGrok Heavy")
    t.to_ascii_lowercase().replace(' ', "_") == "supergrok_heavy"
}

/// URL for upgrading the subscription tier.
pub(crate) const UPSELL_URL_UPGRADE: &str = "https://grok.com/supergrok?referrer=Distill";

/// URL for managing pay-as-you-go or on-demand spending and purchasing credits.
pub(crate) const UPSELL_URL_PAYG: &str = "https://grok.com?_s=usage";

/// Billing mode for credit-limit upsell copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CreditLimitUpsellMode {
    /// Unified usage pool: suggest purchasing prepaid credits.
    UnifiedCredits,
    /// Legacy on-demand PAYG (`enabled` means the on-demand cap is already active).
    LegacyPayg { enabled: bool },
}

/// Resolve upsell copy mode from credits config.
/// A positive `pay_as_you_go` (an on-demand cap over 0) only selects legacy when the unified flag is absent.
/// Unknown defaults to unified (buy credits) so pool users are never told to enable on-demand.
pub(super) fn credit_limit_upsell_mode(
    balance: Option<&crate::views::credit_bar::CreditBalance>,
) -> CreditLimitUpsellMode {
    match balance {
        Some(b) if b.is_unified_billing_user == Some(true) => CreditLimitUpsellMode::UnifiedCredits,
        Some(b) if b.is_unified_billing_user == Some(false) => CreditLimitUpsellMode::LegacyPayg {
            enabled: b.pay_as_you_go,
        },
        // Flag absent: only treat as legacy PAYG on a positive on-demand cap (`pay_as_you_go` derives from that cap)
        Some(b) if b.pay_as_you_go => CreditLimitUpsellMode::LegacyPayg { enabled: true },
        _ => CreditLimitUpsellMode::UnifiedCredits,
    }
}

/// Whether an API or retry error is a credit-limit or spend-block denial.
/// 402 Payment Required always means a credit or spend block here (Build pool and IC spend blocks); no message filter.
/// 403 counts only when the body contains "run out of credits" (legacy IC spend wording); other 403s (content-safety, ZDR, …) are excluded.
pub(crate) fn is_credit_limit_error(http_status: Option<u16>, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    let legacy = m.contains("run out of credits");
    match http_status {
        Some(402) => true,
        Some(403) if legacy => true,
        // Retry notifications embed "status 402" / "status 403" in the body without a separate status field
        None | Some(_) => m.contains("status 402") || (m.contains("status 403") && legacy),
    }
}

/// Option id for Try Again. Submit routes on this sentinel, not on position in the telemetry `choices` vec.
/// position in the telemetry `choices` vec.
pub(crate) const CREDIT_LIMIT_RETRY_OPTION_ID: &str = "retry-last-prompt";

struct CreditLimitCopy {
    heading: &'static str,
    upgrade_tier_desc: &'static str,
    secondary_label: &'static str,
    secondary_desc: &'static str,
    second_choice: distill_telemetry::events::CreditLimitChoice,
    payg_telemetry: bool,
}

/// Open the credit-limit upsell Q&A on the given agent.
/// Non-max-tier: Upgrade tier + buy-credits (or PAYG) + Try Again.
/// Max-tier (SuperGrok Heavy): buy-credits (or PAYG) + Try Again — no upgrade option. URL options carry the target in `id` so the submit handler is position-independent.
pub(super) fn open_credit_limit_upsell(
    agent: &mut AgentView,
    mode: CreditLimitUpsellMode,
    max_tier: bool,
) {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};

    if agent.question_view.is_some() {
        return;
    }

    let copy = match mode {
        CreditLimitUpsellMode::UnifiedCredits => CreditLimitCopy {
            heading: "You hit your weekly limit.",
            upgrade_tier_desc: "Upgrade to a higher tier for more usage",
            secondary_label: "Buy more credits",
            secondary_desc: "Purchase credits to keep using Distill",
            second_choice: distill_telemetry::events::CreditLimitChoice::PurchaseCredits,
            payg_telemetry: false,
        },
        CreditLimitUpsellMode::LegacyPayg { enabled: true } => CreditLimitCopy {
            heading: "You\u{2019}ve hit your spending cap.",
            upgrade_tier_desc: "Upgrade to a higher tier for more credits",
            secondary_label: "Increase limit",
            secondary_desc: "Raise your pay-as-you-go spending cap",
            second_choice: distill_telemetry::events::CreditLimitChoice::PayAsYouGo,
            payg_telemetry: true,
        },
        CreditLimitUpsellMode::LegacyPayg { enabled: false } => CreditLimitCopy {
            heading: "You\u{2019}ve hit the credit limit for your plan.",
            upgrade_tier_desc: "Upgrade to a higher tier for more credits",
            secondary_label: "Pay as you go",
            secondary_desc: "Enable pay-as-you-go credits for on-demand usage",
            second_choice: distill_telemetry::events::CreditLimitChoice::PayAsYouGo,
            payg_telemetry: false,
        },
    };
    let unified_billing = matches!(mode, CreditLimitUpsellMode::UnifiedCredits);

    log_event(distill_telemetry::events::CreditLimitUpsellShown {
        surface: distill_telemetry::events::CreditLimitUpsellSurface::QuestionModal,
        max_tier,
        pay_as_you_go: copy.payg_telemetry,
        unified_billing,
    });

    let mut options = Vec::new();
    let mut choices = Vec::new();
    if !max_tier {
        options.push(QuestionOption {
            label: "Upgrade tier".into(),
            description: copy.upgrade_tier_desc.into(),
            preview: None,
            id: Some(UPSELL_URL_UPGRADE.into()),
        });
        choices.push(distill_telemetry::events::CreditLimitChoice::UpgradeTier);
    }
    options.push(QuestionOption {
        label: copy.secondary_label.into(),
        description: copy.secondary_desc.into(),
        preview: None,
        id: Some(UPSELL_URL_PAYG.into()),
    });
    choices.push(copy.second_choice);
    options.push(QuestionOption {
        label: "Try Again".into(),
        description: "Resubmit the last prompt once you have usage again".into(),
        preview: None,
        id: Some(CREDIT_LIMIT_RETRY_OPTION_ID.into()),
    });
    choices.push(distill_telemetry::events::CreditLimitChoice::RetryLastPrompt);

    let question = Question {
        question: copy.heading.into(),
        options,
        multi_select: Some(false),
        id: None,
    };

    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("credit-limit-upsell-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::CreditLimitUpsell { choices })
    .with_no_freeform();
    agent.install_local_question(state);
    agent.prompt.set_text("");
}

/// Open the free-usage paywall on the given agent: a Q&A modal in the [`open_credit_limit_upsell`] style with two upgrade options.
/// Each option's `id` carries its target URL so the submit handler is position-independent.
/// Only the driver can reach this: the PromptResponse handler calls it, and viewers never receive that response.
pub(super) fn open_free_usage_upsell(agent: &mut AgentView, auth_method: Option<String>) {
    open_supergrok_upsell(agent, UpsellReason::FreeUsageLimit, auth_method);
}

/// Open the SuperGrok upsell for a tier-restricted slash command (`/usage`, `/imagine`, …).
/// Returns whether the modal opened (`false` when another question modal is already up).
/// The caller uses that to decide whether to consume the input that triggered it.
pub(super) fn open_restricted_command_upsell(
    agent: &mut AgentView,
    auth_method: Option<String>,
) -> bool {
    open_supergrok_upsell(agent, UpsellReason::RestrictedCommand, auth_method)
}

/// Which situation opened the SuperGrok upsell modal; it controls the heading and the telemetry source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UpsellReason {
    /// Free-usage quota exhausted (429 paywall).
    FreeUsageLimit,
    /// A tier-restricted slash command was invoked.
    RestrictedCommand,
}

/// Shared builder behind [`open_free_usage_upsell`] and [`open_restricted_command_upsell`]: a Q&A modal in the [`open_credit_limit_upsell`] style.
/// Upgrade options carry their target URL in the option `id`, so submit handling does not depend on option position.
fn open_supergrok_upsell(
    agent: &mut AgentView,
    reason: UpsellReason,
    auth_method: Option<String>,
) -> bool {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};

    // Never displace an already-open question modal
    // Callers that consume input on open must check this `false` and keep the input instead
    if agent.question_view.is_some() {
        return false;
    }

    let (heading, source, modal_id_prefix) = match reason {
        UpsellReason::FreeUsageLimit => (
            "You hit your free usage limit.",
            SuperGrokUpsell::FreeUsagePaywall,
            "free-usage-upsell",
        ),
        UpsellReason::RestrictedCommand => (
            "Unlock all features with SuperGrok.",
            SuperGrokUpsell::RestrictedCommand,
            "restricted-command-upsell",
        ),
    };

    log_event(distill_telemetry::events::SuperGrokUpsellShown {
        source,
        auth_method,
    });

    // /supergrok lists all plans; every upgrade option lands there.
    let options = vec![
        QuestionOption {
            label: "Upgrade to SuperGrok".into(),
            description: "For everyday coding and productivity tasks".into(),
            preview: None,
            id: Some(UPSELL_URL_UPGRADE.into()),
        },
        QuestionOption {
            label: "Upgrade to SuperGrok Plus".into(),
            description: "Significantly higher usage and rate limits".into(),
            preview: None,
            id: Some(UPSELL_URL_UPGRADE.into()),
        },
        QuestionOption {
            label: "Upgrade to SuperGrok Heavy".into(),
            description: "Get the most out of Distill. Highest usage limits.".into(),
            preview: None,
            id: Some(UPSELL_URL_UPGRADE.into()),
        },
    ];
    let question = Question {
        question: heading.into(),
        options,
        multi_select: Some(false),
        id: None,
    };

    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("{modal_id_prefix}-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::FreeUsageUpsell { source })
    .with_no_freeform();
    agent.install_local_question(state);
    agent.prompt.set_text("");
    true
}

/// Apply an [`AutoTopupFetch`] outcome to a cached `auto_topup` slot.
/// `Resolved` sets it, `Cleared` resets it to "unknown" (no credits), and `Unchanged` keeps the last-known-good value (the fetch failed).
pub(super) fn apply_auto_topup(
    slot: &mut Option<crate::views::credit_bar::AutoTopupInfo>,
    fetch: &crate::views::credit_bar::AutoTopupFetch,
) {
    use crate::views::credit_bar::AutoTopupFetch;
    match fetch {
        AutoTopupFetch::Resolved(rule) => *slot = Some(rule.clone()),
        AutoTopupFetch::Cleared => *slot = None,
        AutoTopupFetch::Unchanged => {}
    }
}

// TaskResult handlers.

pub(super) fn handle_billing_fetched(
    app: &mut AppView,
    agent_id: AgentId,
    balance: Option<crate::views::credit_bar::CreditBalance>,
    silent: bool,
    subscription_tier: Option<String>,
    autotopup: crate::views::credit_bar::AutoTopupFetch,
    nonce: u64,
) -> Vec<Effect> {
    // Parse/transport failures route to `BillingError`, so a `None` balance here means the response carried no billing config
    // Clear the cached balance and polling so the status bar agrees with the "No billing data available." message rather than showing a stale value
    app.credit_balance = balance.clone();
    apply_auto_topup(&mut app.auto_topup, &autotopup);
    app.billing_poll_wanted = balance
        .as_ref()
        .map(|b| b.usage_pct >= 99.0)
        .unwrap_or(false);
    if let Some(tier) = subscription_tier {
        app.subscription_tier = Some(tier);
    }
    // Render the `/usage` summary from the now-current cached rule.
    let summary_topup = app.auto_topup.clone();
    let tier_now = app.subscription_tier.clone();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        // Gateway/chat-kind: do not attach Build coding credits.
        let mut topup = agent.auto_topup.clone();
        apply_auto_topup(&mut topup, &autotopup);
        agent.apply_credit_balance(balance.clone(), topup);
        // The open usage modal renders from the mirrors updated above
        // Only its own fetch generation may settle the loading/error flags (background refreshes carry nonce 0)
        if let Some(state) = super::status::usage_modal_state_mut(agent)
            && state.fetch_nonce == nonce
        {
            state.billing_loading = false;
            state.billing_error = None;
            state.ctx.subscription_tier = tier_now;
        }
        if !silent && !agent.chat_kind {
            let msg = match &balance {
                Some(bal) => {
                    crate::views::credit_bar::format_usage_summary(bal, summary_topup.as_ref())
                }
                None => "No billing data available.".to_string(),
            };
            agent.scrollback.push_block(RenderBlock::System(
                crate::scrollback::blocks::SystemMessageBlock::new(msg),
            ));
        }
    }
    vec![]
}

pub(super) fn handle_gate_refreshed(
    app: &mut AppView,
    settings: Option<distill_shell::util::config::RemoteSettings>,
) -> Vec<Effect> {
    let Some(rs) = settings else {
        return vec![];
    };
    app.usage_billing_redirect_url = rs.usage_billing_redirect_url.clone();
    if let Some(secs) = rs.subscription_watch_interval_secs {
        app.subscription_watch_interval_secs = Some(secs);
    }
    match AppView::gate_from_settings(&rs) {
        Some(gate) => app.impose_gate(gate),
        None => app.lift_gate(),
    }
}

/// `x.ai/auth/check_subscription` completed.
/// A failed check only promotes the deferred gate it was verifying (the `verify` generation).
/// Generic watch, focus, and paywall-chain failures never touch it.
pub(super) fn handle_check_subscription_complete(
    app: &mut AppView,
    verify: Option<u64>,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    let was_blocked = !app.has_access();
    let applied = match meta {
        Some(meta_val) => {
            match serde_json::from_value::<distill_login::AuthMeta>(meta_val) {
                Ok(auth_meta) => {
                    app.apply_auth_meta(&auth_meta);
                    true
                }
                Err(e) => {
                    // The shell sent meta we can't decode, a protocol bug rather than a transient failure
                    // The check result is lost, so a verify deferral falls through to promotion below
                    crate::unified_log::error(
                        "subscription.check.meta_parse_failed",
                        None,
                        Some(serde_json::json!({
                            "verify": verify,
                            "error": e.to_string(),
                        })),
                    );
                    false
                }
            }
        }
        // A `None` meta means the shell reports "not authenticated" or the check RPC failed (already logged as subscription.check.rpc_failed)
        None => false,
    };
    if !applied && let Some(generation) = verify {
        app.promote_deferred_gate(generation, "check_failed");
    }
    crate::unified_log::info(
        "subscription.check.complete",
        None,
        Some(serde_json::json!({
            "verify": verify,
            "meta_applied": applied,
            "was_blocked": was_blocked,
            "gated": !app.has_access(),
            "tier": app.subscription_tier,
        })),
    );
    maybe_start_paywall_chain(app, was_blocked)
}

/// Safety net for a hung verification check: show the still-pending deferred gate, erring on the side of blocking.
pub(super) fn handle_gate_verify_timeout(app: &mut AppView, generation: u64) -> Vec<Effect> {
    let was_blocked = !app.has_access();
    app.promote_deferred_gate(generation, "verify_timeout");
    maybe_start_paywall_chain(app, was_blocked)
}

/// Start the 5s paywall auto-check chain when the app goes from ungated to gated.
/// A paywall shown after a failed verification check then lifts itself exactly like the one shown at login.
/// The guard keeps repeated checks and steady-state paywall-poller responses from starting extra timers.
fn maybe_start_paywall_chain(app: &mut AppView, was_blocked: bool) -> Vec<Effect> {
    if !was_blocked && !app.has_access() && app.paywall_check_started.is_none() {
        app.paywall_check_started = Some(std::time::Instant::now());
        return vec![Effect::SchedulePaywallCheck];
    }
    vec![]
}

pub(super) fn handle_credit_limit_recheck_complete(
    app: &mut AppView,
    agent_id: AgentId,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    let old_tier = app.subscription_tier.clone();
    if let Some(meta_val) = meta
        && let Ok(auth_meta) = serde_json::from_value::<distill_login::AuthMeta>(meta_val)
    {
        app.apply_auth_meta(&auth_meta);
    }
    let tier_changed = app.subscription_tier != old_tier && app.subscription_tier.is_some();

    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };

    // If the user already submitted another prompt while the recheck ran, don't retry the stashed one; they've moved on The tier update (above) still takes effect
    // The tier update (above) still takes effect
    let user_moved_on = !agent.session.state.is_idle() || !agent.session.pending_prompts.is_empty();

    if tier_changed && !user_moved_on {
        if let Some(prompt) = agent.credit_limit_stashed_prompt.take() {
            let tier_name = app.subscription_tier.as_deref().unwrap_or("a higher tier");
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Subscription upgraded to {tier_name}. Retrying\u{2026}"
            )));
            agent.session.enqueue_in_flight_prompt_front(prompt);
        }
    } else if !user_moved_on {
        let balance = agent
            .credit_balance
            .as_ref()
            .or(app.credit_balance.as_ref());
        let mode = credit_limit_upsell_mode(balance);
        let max_tier = is_max_tier(app.subscription_tier.as_deref());
        open_credit_limit_upsell(agent, mode, max_tier);
        // Keep the stashed prompt so Try Again can resubmit after the user buys credits or the limit resets.
        // user buys credits or the limit resets.
    } else {
        agent.credit_limit_stashed_prompt = None;
    }

    let mut drain = maybe_drain_queue(agent);
    drain.effects.push(Effect::FetchBilling {
        agent_id,
        silent: true,
        nonce: Default::default(),
    });
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

/// Resubmit the prompt that hit the credit limit (modal option or card button).
pub(super) fn dispatch_retry_credit_limit_prompt(app: &mut AppView) -> Vec<Effect> {
    use crate::app::app_view::ActiveView;

    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    let Some(prompt) = agent.credit_limit_stashed_prompt.take() else {
        agent.show_toast("No prompt to retry.");
        agent
            .scrollback
            .push_block(RenderBlock::system("No prompt to retry."));
        return vec![];
    };
    agent.session.enqueue_in_flight_prompt_front(prompt);
    let drain = maybe_drain_queue(agent);
    if drain.effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SendPrompt { .. }
                | Effect::SendPromptBlocks { .. }
                | Effect::SendBashCommand { .. }
        )
    }) {
        agent
            .scrollback
            .push_block(RenderBlock::system("Trying again\u{2026}"));
    }
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

// Action handlers.

pub(super) fn dispatch_open_supergrok_url(app: &mut AppView) -> Vec<Effect> {
    log_event(SuperGrokUpsellClicked {
        source: SuperGrokUpsell::WelcomeScreen,
        auth_method: app.login_method_id.as_ref().map(|id| id.0.to_string()),
    });
    let url = app
        .gate
        .as_ref()
        .and_then(|g| g.url.as_deref())
        .unwrap_or("https://grok.com/supergrok?referrer=Distill");
    // Funnel attribution: tag SuperGrok upsell clicks from the CLI with `referrer=Distill`, matching the OAuth consent flow and x.ai/cli links
    // It applies even when the URL came from remote settings's `gate_url`, so nothing depends on the remote flag being configured correctly
    // If the URL already specifies a referrer it's left alone
    let url = crate::app::link_opener::ensure_query_param(url, "referrer", "Distill");
    super::ctx::open_url_or_show(app, &url);
    vec![]
}
