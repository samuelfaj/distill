// Modified for Distill by Samuel Fajreldines, 2026.
//! Checks the live subscription tier so the paywall gate can lift.
//!
//! `single_check()` queries `GET /user?include=subscription` on the backend for the live tier, independent of the JWT.
//! On a qualifying tier it does a best-effort JWT refresh and returns an `UnblockResult`.
//! The agent then re-fetches settings and lifts the gate itself.
//!
//! The pager drives the polling via `x.ai/auth/check_subscription`.
//! The callers are the 5s paywall chain, the free-tier watch, the refocus check, and the gate deferral that verifies before showing the paywall.
//! See the pager's `app::subscription` module.
use distill_login::AuthManager;
use distill_login::UserInfo;
use distill_login::manager::{BEST_EFFORT_REFRESH_TIMEOUT, BoundedRefresh, RefreshReason};
use distill_login::token_type::TokenType;
use std::sync::Arc;
use std::time::Duration;
/// Any active subscription qualifies: the proxy only returns a tier when an active subscription exists (`None` otherwise).
/// The access gate in remote settings controls which tiers are actually allowed.
/// The `"Free"` guard is defense-in-depth should the proxy ever start stamping free users explicitly.
fn is_qualifying_tier(tier: &str) -> bool {
    !tier.is_empty() && tier != "Free"
}
/// Returned only when the check confirmed a qualifying tier.
pub(crate) struct UnblockResult {
    pub(crate) new_tier: String,
    /// The `userId` from the `/user` response that confirmed the tier, resolved with the live bearer so it names the account the check started with. The caller's identity guard accepts it alongside the started user_id.
    /// The mint below spawns a `/user` enrichment that can rewrite a seeded or stale user_id to this canonical value mid-check. That rewrite is not an account switch.
    pub(crate) canonical_user_id: String,
    /// True when the best-effort refresh below hit its bounded deadline with the exchange still running (detached, not dropped).
    /// The caller must not force a second mint then: it would only queue behind the detached exchange for up to another full budget.
    /// That would hold the gate lift past the single budget while the subscription is already confirmed.
    pub(crate) refresh_deadline_hit: bool,
}
async fn fetch_user_info(
    http_client: &reqwest::Client,
    url: &str,
    auth: &distill_login::GrokAuth,
    auth_manager: &AuthManager,
    alpha_test_key: Option<&str>,
) -> Result<UserInfo, &'static str> {
    let request = http_client
        .get(url)
        .timeout(Duration::from_secs(10))
        .header("Authorization", format!("Bearer {}", auth.key))
        .header(
            "X-XAI-Token-Auth",
            auth_manager.grok_com_config().token_header.as_str(),
        )
        .header("x-grok-client-version", distill_version::VERSION)
        .header(
            crate::http::CLIENT_MODE_HEADER,
            crate::http::process_client_mode(),
        );
    let _ = alpha_test_key;
    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<UserInfo>().await.map_err(|_| "parse")
        }
        Ok(_resp) => Err("http_status"),
        Err(e) if e.is_timeout() => Err("timeout"),
        Err(_) => Err("transport"),
    }
}
/// Called by the pager every 5s while the paywall is shown (`x.ai/auth/check_subscription`).
/// Queries `/user?include=subscription` for the live tier.
/// On a qualifying tier it does a best-effort JWT refresh and returns `Some(UnblockResult)`.
#[tracing::instrument(name = "auth.paywall_check", skip_all, fields(user_id = %user_id))]
pub(crate) async fn single_check(
    auth_manager: Arc<AuthManager>,
    proxy_base_url: &str,
    alpha_test_key: Option<&str>,
    user_id: &str,
) -> Option<UnblockResult> {
    use distill_login::backend::{ActiveAuthBackend, AuthBackend};
    if !ActiveAuthBackend::default().is_xai_authority() {
        return None;
    }
    let user_url = format!("{}/user?include=subscription", proxy_base_url);
    let http_client = crate::http::shared_client();
    let auth = auth_manager.current()?;
    let user_info = match fetch_user_info(
        &http_client,
        &user_url,
        &auth,
        &auth_manager,
        alpha_test_key,
    )
    .await
    {
        Ok(ui) => ui,
        Err(kind) => {
            distill_telemetry::unified_log::warn(
                "paywall_check_error",
                None,
                Some(serde_json::json!({ "user_id": user_id, "kind": kind })),
            );
            return None;
        }
    };
    distill_telemetry::unified_log::info(
        "paywall_check_result",
        None,
        Some(serde_json::json!({
            "user_id": user_id,
            "subscription_tier": user_info.subscription_tier,
        })),
    );
    let new_tier = match &user_info.subscription_tier {
        Some(tier) if !tier.is_empty() => tier.clone(),
        _ => return None,
    };
    if !is_qualifying_tier(&new_tier) {
        return None;
    }
    distill_telemetry::unified_log::info(
        "paywall_check_subscription_detected",
        None,
        Some(serde_json::json!({
            "user_id": user_id,
            "new_tier": new_tier,
        })),
    );
    let refresh_deadline_hit = match auth_manager
        .refresh_chain_bounded_outcome(
            TokenType::OidcSession,
            RefreshReason::ServerRejected,
            BEST_EFFORT_REFRESH_TIMEOUT,
        )
        .await
    {
        BoundedRefresh::Resolved(result) => {
            if let Err(e) = *result {
                distill_telemetry::unified_log::warn(
                    "paywall_check_error",
                    None,
                    Some(serde_json::json!({
                        "user_id": user_id,
                        "kind": "refresh_failed",
                        "detail": e.to_string(),
                    })),
                );
            }
            false
        }
        BoundedRefresh::DeadlineElapsed => {
            distill_telemetry::unified_log::warn(
                "paywall_check_error",
                None,
                Some(serde_json::json!({
                    "user_id": user_id,
                    "kind": "refresh_deadline",
                    "detail": "bounded refresh deadline elapsed; mint continues in background",
                })),
            );
            true
        }
    };
    distill_telemetry::unified_log::info(
        "paywall_check_unblocked",
        None,
        Some(serde_json::json!({ "user_id": user_id, "new_tier": new_tier })),
    );
    Some(UnblockResult {
        new_tier,
        canonical_user_id: user_info.user_id,
        refresh_deadline_hit,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_paid_tiers_qualify() {
        for tier in &[
            "SuperGrokPro",
            "SuperGrokPlus",
            "GrokPro",
            "SuperGrokLite",
            "XPremiumPlus",
            "XPremium",
            "XBasic",
        ] {
            assert!(is_qualifying_tier(tier), "{tier} must qualify");
        }
    }
    #[test]
    fn free_and_empty_tiers_are_not_qualifying() {
        assert!(!is_qualifying_tier("Free"));
        assert!(!is_qualifying_tier(""));
    }
}
