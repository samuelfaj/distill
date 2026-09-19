// Modified for Distill by Samuel Fajreldines, 2026.
//! Upsell, announcement, and credit-limit product telemetry events.

use serde::Serialize;

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuperGrokUpsell {
    WelcomeScreen,
    RateLimitError,
    /// Free-usage-exhausted paywall modal (free-tier 429 with the `subscription:free-usage-exhausted` well-known error code).
    FreeUsagePaywall,
    /// Upsell modal shown when a tier-restricted slash command (`/usage`, `/imagine`, …) is invoked on the free / X Basic tiers.
    RestrictedCommand,
}

#[derive(Serialize)]
pub struct SuperGrokUpsellShown {
    pub source: SuperGrokUpsell,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

#[derive(Serialize)]
pub struct SuperGrokUpsellClicked {
    pub source: SuperGrokUpsell,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

/// Modeled on [`SuperGrokUpsell`]; lets the funnel attribute the click to the welcome hero vs the in-session header vs
/// the banner vs the dashboard. Also distinguishes keyboard (`Ctrl+O`) activations from pointer/OSC 8 ones. Ord/Eq exist
/// so the pager can track which (announcement, surface) pairs already showed the CTA.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementCtaSurface {
    Banner,
    Welcome,
    Header,
    Dashboard,
    Keyboard,
}

/// A promo announcement's CTA button was painted on a surface: the impression half of the per-surface CTR funnel with [`AnnouncementCtaClicked`].
/// Emitted once per (announcement, surface) per pager process (cleared on logout); never emitted for `Keyboard` (a click-only surface).
#[derive(Serialize)]
pub struct AnnouncementCtaShown {
    /// Announcement `id` from the server push (`None` for id-less items).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Which surface painted the button.
    pub source: AnnouncementCtaSurface,
}

/// User activated a promo announcement's CTA button (the `[label]` open).
#[derive(Serialize)]
pub struct AnnouncementCtaClicked {
    /// Announcement `id` from the server push (`None` for id-less items).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Which surface the activation came from (per-surface conversion signal).
    pub source: AnnouncementCtaSurface,
}

/// 403 "run out of credits": billing exhaustion (not request throttling).
#[derive(Serialize)]
pub struct CreditLimitHit {
    pub model_id: String,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CreditLimitUpsellSurface {
    /// Q&A modal (upgrade, buy / PAYG, and try again; or buy and try again at max-tier).
    QuestionModal,
    /// Retired: max-tier used an inline scrollback card. Kept for historical events.
    InlineCard,
}

/// Credit-limit upsell displayed to the user.
#[derive(Serialize)]
pub struct CreditLimitUpsellShown {
    pub surface: CreditLimitUpsellSurface,
    pub max_tier: bool,
    pub pay_as_you_go: bool,
    /// User is on unified usage billing (buy-credits wording).
    /// When false, legacy on-demand / PAYG wording was used.
    #[serde(default)]
    pub unified_billing: bool,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CreditLimitChoice {
    UpgradeTier,
    /// Covers both "Pay as you go" (enable) and "Increase limit" (raise cap).
    PayAsYouGo,
    /// Unified-billing / credits-pool users: purchase prepaid credits.
    PurchaseCredits,
    /// Resubmit the prompt that hit the credit limit.
    RetryLastPrompt,
}

/// User clicked an option in the credit-limit upsell.
#[derive(Serialize)]
pub struct CreditLimitUpsellClicked {
    pub surface: CreditLimitUpsellSurface,
    pub choice: CreditLimitChoice,
}

/// Emitted when a previously access-gated user re-authenticates and the gate is lifted, i.e. they subscribed (externally on grok.com) and came back.
/// This is the actual conversion signal for SuperGrok Heavy subscriptions attributed to Distill.
/// The user saw the gate in Distill, went and paid, then returned with access.
#[derive(Serialize)]
pub struct SubscriptionActivated {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    /// Whether the subscribe CTA was shown in this session before the gate was lifted (`access_gate_shown_logged`).
    /// When `true`, the conversion is strongly attributable to Distill's upsell surface.
    pub upsell_shown_this_session: bool,
}
