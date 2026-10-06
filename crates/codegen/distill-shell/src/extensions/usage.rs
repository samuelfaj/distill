// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/session/usage`: cumulative session token and cost totals as [`PromptUsage`].
//!
//! Reads the in-memory [`distill_chat_state::UsageLedger`] (main-loop and folded subagent spend).
//! Partial costs are scrubbed, since an absent cost does not mean free.
//! Totals reset when a session is resumed in a new agent process.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::extensions::notification::PromptUsage;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionUsageRequest {
    session_id: String,
}

/// Wire response for `x.ai/session/usage`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageResponse {
    pub usage: PromptUsage,
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/session/usage" => handle_session_usage(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_session_usage(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: SessionUsageRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(req.session_id.as_str());

    // Wait out an in-flight session/load so a reconnecting client is not answered with not-found
    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            req.session_id
        ))));
    };

    // Fail closed: a dead chat-state actor is an error, never a zero bill.
    let ledger = handle
        .chat_state_handle
        .try_get_session_usage()
        .await
        .map_err(|()| acp::Error::internal_error().data("failed to read session usage"))?;

    let mut usage = PromptUsage::from(&ledger);
    add_cache_savings(&mut usage, &ledger);
    to_raw_response(&SessionUsageResponse { usage })
}

/// Fills the estimated net cache savings: what each call's cache reads would have cost as uncached
/// input, less the premium its cache writes paid over it. Only calls to OpenRouter are priced, from its published per-token prices (the same model
/// facts routing reads); a direct provider or a subscription has other terms, so its rows get none.
fn add_cache_savings(usage: &mut PromptUsage, ledger: &distill_chat_state::UsageLedger) {
    let priced: Vec<_> = ledger
        .attributions
        .iter()
        .filter_map(|row| {
            let endpoint = row.endpoint.as_deref()?;
            let usage = row.usage.as_ref()?;
            let (cached, written) = (
                usage.cached_prompt_tokens,
                usage.cache_creation_prompt_tokens,
            );
            ((cached > 0 || written > 0) && crate::openrouter_auth::is_openrouter_url(endpoint))
                .then_some((
                    row.model_id.as_str(),
                    endpoint,
                    (u64::from(cached), u64::from(written)),
                ))
        })
        .collect();
    if priced.is_empty() {
        return;
    }
    let candidates: Vec<_> = priced
        .iter()
        .map(|(model, endpoint, _)| (*model, *endpoint))
        .collect();
    let facts = crate::jev_model_facts::model_facts(&candidates);
    for ((model, _, (cached, written)), facts) in priced.iter().zip(&facts) {
        let pricing = facts
            .get("openrouter")
            .and_then(|facts| facts.get("pricing"))
            .unwrap_or(&serde_json::Value::Null);
        let Some(ticks) = cache_savings_ticks(*cached, *written, pricing) else {
            continue;
        };
        for row in [Some(&mut usage.totals), usage.model_usage.get_mut(*model)]
            .into_iter()
            .flatten()
        {
            row.cache_savings_usd_ticks = Some(
                row.cache_savings_usd_ticks
                    .unwrap_or(0)
                    .saturating_add(ticks),
            );
        }
    }
}

/// `cached` tokens at the input price minus the cache-read price, less `written` tokens at the
/// cache-write price minus the input price, in USD ticks: negative when the writes cost more than
/// the reads saved. `None` unless the input and read prices are known (OpenRouter publishes them as
/// decimal strings, USD per token); a write price it does not publish is no premium.
fn cache_savings_ticks(cached: u64, written: u64, pricing: &serde_json::Value) -> Option<i64> {
    let price = |key: &str| -> Option<f64> {
        match &pricing[key] {
            serde_json::Value::String(text) => text.parse().ok(),
            value => value.as_f64(),
        }
    };
    let prompt = price("prompt")?;
    let saved_per_read = prompt - price("input_cache_read")?;
    let premium_per_write = price("input_cache_write").map_or(0.0, |write| (write - prompt).max(0.0));
    let net = cached as f64 * saved_per_read - written as f64 * premium_per_write;
    if !net.is_finite() || saved_per_read < 0.0 || net == 0.0 {
        return None;
    }
    let ticks = distill_sampling_types::usd_cost_to_ticks(net.abs())?;
    Some(if net < 0.0 { -ticks } else { ticks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_chat_state::UsageLedger;
    use distill_sampling_types::TokenUsage;

    fn usage(prompt: u32, completion: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: 0,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
            cache_creation_1h_prompt_tokens: 0,
        }
    }

    #[test]
    fn response_serializes_ledger_as_prompt_usage_wire_shape() {
        let mut ledger = UsageLedger::default();
        ledger.record_main_loop_call("Distill", &usage(100, 10), Some(50), Some(20_000_000));
        let v = serde_json::to_value(&SessionUsageResponse {
            usage: PromptUsage::from(&ledger),
        })
        .unwrap();
        assert_eq!(
            v.pointer("/usage/inputTokens"),
            Some(&serde_json::json!(100))
        );
        assert_eq!(
            v.pointer("/usage/outputTokens"),
            Some(&serde_json::json!(10))
        );
        assert_eq!(v.pointer("/usage/numTurns"), Some(&serde_json::json!(1)));
        assert_eq!(
            v.pointer("/usage/costUsdTicks"),
            Some(&serde_json::json!(20_000_000))
        );
        assert_eq!(
            v.pointer("/usage/modelUsage/Distill/inputTokens"),
            Some(&serde_json::json!(100))
        );
        let rt: SessionUsageResponse = serde_json::from_value(v).unwrap();
        assert_eq!(rt.usage.totals.cost_usd_ticks, Some(20_000_000));
    }

    /// The savings line is what a cache hit was worth: the cached tokens at the uncached input
    /// price minus what the read cost. Without both prices it stays unknown, never zero.
    #[test]
    fn cache_savings_need_both_prices() {
        let pricing = serde_json::json!({"prompt": "0.000003", "input_cache_read": "0.0000003"});
        // 1M cached tokens at $3/M input vs $0.30/M read saves $2.70.
        assert_eq!(
            cache_savings_ticks(1_000_000, 0, &pricing),
            Some(27_000_000_000)
        );
        assert_eq!(
            cache_savings_ticks(1_000_000, 0, &serde_json::json!({"prompt": "0.000003"})),
            None
        );
        assert_eq!(
            cache_savings_ticks(1_000_000, 0, &serde_json::Value::Null),
            None
        );
        // A read priced at or above input saved nothing worth showing.
        assert_eq!(
            cache_savings_ticks(
                10,
                0,
                &serde_json::json!({"prompt": "0.000001", "input_cache_read": "0.000001"})
            ),
            None
        );
    }

    /// A session that keeps breaking its cache pays the write premium (1.25x on Anthropic via
    /// OpenRouter) far more than its reads save: the figure is net, so it shows the loss instead
    /// of a saving.
    #[test]
    fn cache_savings_are_net_of_the_write_premium() {
        let pricing = serde_json::json!({
            "prompt": "0.000003",
            "input_cache_read": "0.0000003",
            "input_cache_write": "0.00000375",
        });
        // 20k read save 20k x $2.70/M = $0.054; 200k written cost 200k x $0.75/M = $0.15.
        assert_eq!(
            cache_savings_ticks(20_000, 200_000, &pricing),
            Some(-960_000_000)
        );
        // Without writes it is the read saving alone.
        assert_eq!(
            cache_savings_ticks(20_000, 0, &pricing),
            Some(540_000_000)
        );
    }

    /// Only OpenRouter calls are priced: a direct or subscription endpoint has other terms.
    #[test]
    fn calls_off_openrouter_get_no_savings() {
        let mut ledger = UsageLedger::default();
        let mut call = usage(100, 10);
        call.cached_prompt_tokens = 80;
        ledger.record_attribution(distill_chat_state::UsageAttribution {
            attempt_id: "a1".into(),
            task_id: None,
            turn_id: None,
            request_id: None,
            role: "main".into(),
            model_id: "claude-direct".into(),
            endpoint: Some("https://api.anthropic.com/v1/messages".into()),
            requested_effort: None,
            applied_effort: None,
            reason: None,
            bytes_in: None,
            bytes_out: None,
            source_kind: None,
            final_decision: None,
            status: distill_chat_state::UsageCallStatus::Completed,
            usage: Some(call),
            usage_complete: true,
            api_duration_ms: None,
            cost_usd_ticks: None,
            cost_basis: distill_chat_state::UsageCostBasis::Unknown,
        });
        let mut projected = PromptUsage::from(&ledger);
        add_cache_savings(&mut projected, &ledger);
        assert_eq!(projected.totals.cache_savings_usd_ticks, None);
    }

    #[test]
    fn response_scrubs_partial_costs() {
        let mut ledger = UsageLedger::default();
        ledger.record_main_loop_call("a", &usage(100, 10), None, Some(70));
        ledger.record_main_loop_call("a", &usage(50, 5), None, None);
        let v = serde_json::to_value(&SessionUsageResponse {
            usage: PromptUsage::from(&ledger),
        })
        .unwrap();
        // Scrubbed cost is either omitted or serialized as null.
        assert!(
            v.pointer("/usage/costUsdTicks")
                .is_none_or(serde_json::Value::is_null),
            "{v:?}"
        );
        assert_eq!(
            v.pointer("/usage/costIsPartial"),
            Some(&serde_json::json!(true))
        );
    }
}
