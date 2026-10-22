// Modified for Distill by Samuel Fajreldines, 2026.
//! B2 (tier): in UltraSave and UltraFast one Jev decision per main round picks
//! which of the user's three chosen models runs it: main (the session model),
//! worker, or utility. Nothing outside those three is ever a candidate.

use distill_workspace::jev::catalog::routing;
use distill_workspace::jev::flags::JevLever;

use super::*;

pub(super) const TIER_QUESTION: &str = "execution_tier";

/// The user's three models, named by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Tier {
    Utility,
    Worker,
    Main,
}

impl Tier {
    pub(super) const fn id(self) -> &'static str {
        match self {
            Self::Utility => "utility",
            Self::Worker => "worker",
            Self::Main => "main",
        }
    }
}

/// A tier and the model id it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TierEntry {
    pub(super) tier: Tier,
    pub(super) model: String,
}

/// One entry per distinct model, cheapest first (utility, worker, main).
/// `entries` is in priority order (main, worker, utility): when two roles name
/// the same model the higher-priority role keeps it, so the main model is
/// never replaced by an alias of itself.
pub(super) fn dedupe_tiers(entries: Vec<TierEntry>) -> Vec<TierEntry> {
    let mut kept: Vec<TierEntry> = Vec::new();
    for entry in entries {
        if !kept.iter().any(|other| other.model == entry.model) {
            kept.push(entry);
        }
    }
    kept.sort_by_key(|entry| entry.tier);
    kept
}

/// The tier Jev picked. Probability mass is read cheapest first, so an even
/// split lands on the cheaper model; labels outside the candidates are ignored.
pub(super) fn compose_tier(
    answers: &distill_workspace::jev::JevAnswerSet,
    entries: &[TierEntry],
) -> Option<Tier> {
    let offered: Vec<routing::EffortChoice> = entries
        .iter()
        .map(|entry| routing::EffortChoice {
            id: entry.tier.id().to_owned(),
            description: String::new(),
        })
        .collect();
    let picked = routing::compose_micro_effort_for(answers, &offered, TIER_QUESTION)?;
    entries
        .iter()
        .find(|entry| entry.tier.id() == picked)
        .map(|entry| entry.tier)
}

fn tier_policy(mode: distill_tool_types::ExecutionMode) -> &'static str {
    if mode == distill_tool_types::ExecutionMode::Ultrasave {
        "UltraSave: choose the cheapest of the user's chosen models that still completes this step correctly. Count total money including retries and rework. A small quality loss is acceptable; a critical one is not."
    } else {
        "UltraFast: choose the fastest of the user's chosen models (lowest latency, highest throughput) that still completes this step correctly. A small quality loss is acceptable; a critical one is not."
    }
}

fn tier_instructions(mode: distill_tool_types::ExecutionMode) -> &'static str {
    if mode == distill_tool_types::ExecutionMode::Ultrasave {
        "Pick the cheapest of the user's chosen models that will still complete this step correctly. Default to the cheapest candidate for reading, searching, editing from a clear plan, running checks and fixing a known failure; choose a more capable one only when this step needs it (an unexplained failure after a retry, a design decision, a subtle correctness judgment). A small quality loss is acceptable, a critical one is not. Count retries and rework. When `prefix_cached` is true, switching re-reads the prompt uncached; switch only when the saving clearly outweighs that."
    } else {
        "Pick the fastest of the user's chosen models (lower latency, higher throughput; smaller models are usually faster) that will still complete this step correctly. Default to the fastest candidate for reading, searching, editing from a clear plan, running checks and fixing a known failure; choose a more capable one only when this step needs it (an unexplained failure after a retry, a design decision, a subtle correctness judgment). A small quality loss is acceptable, a critical one is not. Count retries and rework as time. When `prefix_cached` is true, switching re-reads the prompt uncached, which costs time; switch only when the time saved clearly outweighs that."
    }
}

fn tier_questions(
    mode: distill_tool_types::ExecutionMode,
    entries: &[(Tier, String)],
) -> Option<std::collections::BTreeMap<String, distill_workspace::jev::Question>> {
    let criteria = entries
        .iter()
        .map(|(tier, description)| {
            (
                tier.id().to_owned(),
                serde_json::Value::String(description.clone()),
            )
        })
        .collect();
    let question = distill_workspace::jev::Question::choice(
        format!(
            "Which of the user's chosen models should run the single model call described in `micro_action`? {} \
             Judge this step only. Benchmarks and list prices are evidence, not guarantees.",
            tier_instructions(mode)
        ),
        criteria,
    )
    .ok()?;
    Some([(TIER_QUESTION.to_owned(), question)].into())
}

impl SessionActor {
    /// Whether this round may choose among the user's three models.
    pub(super) fn tier_routing_allowed(&self) -> bool {
        matches!(
            self.execution_mode.load(),
            distill_tool_types::ExecutionMode::Ultrafast
                | distill_tool_types::ExecutionMode::Ultrasave
        ) && self
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed)
            && !self.child_model_routing_locked()
            && !self.child_jev_routing_locked()
            && !self.jev_ledger.borrow().tier_route_off
            && crate::jev::lever_active(JevLever::B2ExecutionTier)
    }

    /// Why `tier_routing_allowed` said no, for the skip record.
    fn tier_block_reason(&self) -> &'static str {
        if !matches!(
            self.execution_mode.load(),
            distill_tool_types::ExecutionMode::Ultrafast
                | distill_tool_types::ExecutionMode::Ultrasave
        ) {
            "execution mode is not UltraFast/UltraSave"
        } else if !self
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            "effort is not auto"
        } else if self.child_model_routing_locked() || self.child_jev_routing_locked() {
            "model routing is locked"
        } else if self.jev_ledger.borrow().tier_route_off {
            "tier route off for this turn"
        } else {
            "lever b2_execution_tier is off"
        }
    }

    /// The worker the user chose: the session override, else `[models] worker`.
    fn tier_worker_slug(&self) -> Option<String> {
        let session = self
            .rebuild_spec
            .worker_override
            .read()
            .as_ref()
            .map(|worker| worker.model_id.clone());
        session.unwrap_or_else(crate::jev::worker_model)
    }

    /// Picks this round's model among main, worker and utility. `true` when a
    /// non-main model now runs the round (`cfg` is replaced); `false` keeps
    /// `cfg` on the main model. A failed request later puts the rest of the
    /// turn back on the session model (see `undo_local_route`).
    pub(super) async fn jev_route_execution_tier(&self, cfg: &mut SamplingConfig) -> bool {
        self.jev_ledger.borrow_mut().tier_route_slug = None;
        let skip = |reason: &str| {
            crate::jev::record_item(JevLever::B2ExecutionTier, "skip", reason, None, None);
            distill_telemetry::unified_log::info(
                "jev tier route skipped",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "reason": reason })),
            );
            false
        };
        if !self.tier_routing_allowed() {
            // Normal and UltraCode never route; only the Ultra modes explain a skip.
            return matches!(
                self.execution_mode.load(),
                distill_tool_types::ExecutionMode::Ultrafast
                    | distill_tool_types::ExecutionMode::Ultrasave
            ) && skip(self.tier_block_reason());
        }
        // Account Auto has model authority; Jev must not choose the model there.
        // A pinned route (an explicit `--model`) leaves the model to the user; only
        // an unpinned Auto route, or Auto enabled globally, owns it.
        let route = crate::agent::account_routing::AccountRoute::read(&cfg.extra_headers)
            .ok()
            .flatten();
        if let Some(route) = route.as_ref().filter(|route| !route.pinned) {
            return skip(&format!(
                "account Auto route {:?} owns the model",
                route.option_id
            ));
        }
        if crate::accounts::AccountStore::default()
            .load_routing()
            .is_ok_and(|routing| routing.enabled)
        {
            return skip("account Auto routing is enabled");
        }
        let mode = self.execution_mode.load();
        let creds = self.chat_state_handle.get_credentials().await;
        let conversation = self.chat_state_handle.get_conversation().await;
        let estimate = self.jev_prompt_token_estimate(&conversation).await;
        let resolve = |slug: &str| self.aux_sampler_config_with(slug, &creds);
        let utility_slug =
            crate::jev_cheap::utility_catalog_model(&self.models_manager, &[], &resolve);
        let mut resolved: Vec<(TierEntry, Option<SamplingConfig>)> = vec![(
            TierEntry {
                tier: Tier::Main,
                model: cfg.model.clone(),
            },
            None,
        )];
        let mut slugs: Vec<(Tier, String)> = Vec::new();
        for (tier, slug) in [
            (Tier::Worker, self.tier_worker_slug()),
            (Tier::Utility, utility_slug),
        ] {
            let Some(slug) = slug else {
                skip(&format!("no {} model configured", tier.id()));
                continue;
            };
            match self.resolve_routed_model(&slug, cfg, &creds, &conversation, estimate) {
                Ok(routed) => {
                    slugs.push((tier, slug.clone()));
                    resolved.push((
                        TierEntry {
                            tier,
                            model: routed.config.model.clone(),
                        },
                        Some(routed.config),
                    ));
                }
                Err(error) => {
                    skip(&format!("{} candidate `{slug}` dropped: {error}", tier.id()));
                }
            }
        }
        let entries = dedupe_tiers(resolved.iter().map(|(entry, _)| entry.clone()).collect());
        if entries.len() < 2 {
            return skip("fewer than two distinct usable candidates");
        }
        let config_of = |tier: Tier| -> Option<&SamplingConfig> {
            resolved
                .iter()
                .find(|(entry, _)| entry.tier == tier)
                .and_then(|(_, config)| config.as_ref())
        };
        let candidates: Vec<(Tier, &SamplingConfig)> = entries
            .iter()
            .filter_map(|entry| {
                if entry.tier == Tier::Main {
                    Some((entry.tier, &*cfg))
                } else {
                    config_of(entry.tier).map(|config| (entry.tier, config))
                }
            })
            .collect();
        let facts = crate::jev_model_facts::model_facts(
            &candidates
                .iter()
                .map(|(_, config)| (config.model.as_str(), config.base_url.as_str()))
                .collect::<Vec<_>>(),
        );
        let list_prices = crate::jev_model_facts::catalog_list_pricing(
            &candidates
                .iter()
                .map(|(_, config)| (config.model.as_str(), config.base_url.as_str()))
                .collect::<Vec<_>>(),
        );
        let descriptions: Vec<(Tier, String)> = candidates
            .iter()
            .map(|(tier, config)| {
                let role = match tier {
                    Tier::Main => "the session's main model",
                    Tier::Worker => "the user's worker model",
                    Tier::Utility => "the user's utility model",
                };
                (
                    *tier,
                    format!(
                        "{} ({role}), {} token window",
                        self.model_display_name(&config.model),
                        config.context_window
                    ),
                )
            })
            .collect();
        let Some(questions) = tier_questions(mode, &descriptions) else {
            return skip("tier question could not be built");
        };
        let main_name = self.model_display_name(&cfg.model);
        let mut state = self.micro_effort_state(cfg, &main_name, estimate).await;
        state["step"] = state["micro_action"]["step"].clone();
        state["execution_policy"] = serde_json::json!(tier_policy(mode));
        state["failed_tool_calls_this_turn"] =
            serde_json::json!(self.jev_ledger.borrow().facts.failed_tool_count());
        state["previous_dispatch"] = serde_json::json!(self.jev_ledger.borrow().last_execution);
        state["candidates"] = serde_json::Value::Array(
            candidates
                .iter()
                .zip(facts.into_iter().zip(list_prices))
                .map(|((tier, config), (facts, list_price))| {
                    serde_json::json!({
                        "id": tier.id(),
                        "model_id": config.model,
                        "facts": facts,
                        "list_price_per_token": list_price,
                    })
                })
                .collect(),
        );
        let prefix_cached = self.jev_ledger.borrow().last_main_prompt_tokens().is_some();
        state["prefix_cached"] = serde_json::json!(prefix_cached);
        state["list_price_note"] = serde_json::json!(
            "list_price_per_token is the OpenRouter list price of the same model id; the user's endpoint may bill differently; use only to rank relative cost."
        );
        state["switch_cost"] = serde_json::json!(if prefix_cached {
            "A model different from the previous dispatch re-reads the whole prompt uncached."
        } else {
            "No main request has been sent yet, so there is no cached prefix: switching is free this round."
        });
        let Some(answers) = crate::jev::ask_item(JevLever::B2ExecutionTier, state, questions).await
        else {
            return skip("Jev gave no answer");
        };
        let confidence = answers.confidence(TIER_QUESTION);
        let Some(tier) = compose_tier(&answers, &entries) else {
            crate::jev::record_item(
                JevLever::B2ExecutionTier,
                "defer",
                "no usable tier answer; the main model keeps the round",
                confidence,
                Some(&answers),
            );
            return false;
        };
        if tier == Tier::Main {
            crate::jev::record_item(
                JevLever::B2ExecutionTier,
                "main",
                &format!("model {}", cfg.model),
                confidence,
                Some(&answers),
            );
            return false;
        }
        let Some(mut routed) = config_of(tier).cloned() else {
            return skip("chosen tier has no resolved config");
        };
        // Keep the effort only where the new model offers it; otherwise the
        // effort pass that follows chooses one from its own menu.
        routed.reasoning_effort = cfg.reasoning_effort.filter(|effort| {
            self.models_manager
                .model_supports_reasoning_effort_value(&routed.model, *effort)
        });
        crate::jev::record_item(
            JevLever::B2ExecutionTier,
            tier.id(),
            &format!("model {} for this round", routed.model),
            confidence,
            Some(&answers),
        );
        {
            let mut ledger = self.jev_ledger.borrow_mut();
            ledger.set_pending_route(routed.model.clone());
            ledger.tier_route_active = true;
            ledger.tier_route_touched = true;
            ledger.tier_route_slug = slugs
                .iter()
                .find(|(candidate, _)| *candidate == tier)
                .map(|(_, slug)| slug.clone());
        }
        *cfg = routed;
        true
    }

    /// Re-applies a non-main model an earlier round of the turn chose: the
    /// full endpoint and credentials of that model, not only its id. `false`
    /// when it no longer resolves or fits; the round then decides afresh.
    pub(super) async fn jev_resume_tier_route(
        &self,
        cfg: &mut SamplingConfig,
        slug: &str,
        effort: Option<distill_sampling_types::ReasoningEffort>,
    ) -> bool {
        if self.jev_ledger.borrow().tier_route_off {
            return false;
        }
        let creds = self.chat_state_handle.get_credentials().await;
        let conversation = self.chat_state_handle.get_conversation().await;
        let estimate = self.jev_prompt_token_estimate(&conversation).await;
        let Ok(routed) = self.resolve_routed_model(slug, cfg, &creds, &conversation, estimate)
        else {
            return false;
        };
        let mut routed = routed.config;
        routed.reasoning_effort = effort;
        {
            let mut ledger = self.jev_ledger.borrow_mut();
            ledger.set_pending_route(routed.model.clone());
            ledger.tier_route_active = true;
            ledger.tier_route_touched = true;
            ledger.tier_route_slug = Some(slug.to_owned());
        }
        *cfg = routed;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tier: Tier, model: &str) -> TierEntry {
        TierEntry {
            tier,
            model: model.to_owned(),
        }
    }

    fn answer(choice: &str, probabilities: &[(&str, f64)]) -> distill_workspace::jev::JevAnswerSet {
        distill_workspace::jev::JevAnswerSet {
            model: "test-jev".to_owned(),
            answers: [(
                TIER_QUESTION.to_owned(),
                distill_workspace::jev::Answer::Choice {
                    choice: choice.to_owned(),
                    probabilities: probabilities
                        .iter()
                        .map(|(id, p)| ((*id).to_owned(), *p))
                        .collect(),
                    confidence: Some(0.9),
                },
            )]
            .into(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 0,
        }
    }

    /// Paying for the same model twice under two names is waste, and the main
    /// model must stay main when a role aliases it.
    #[test]
    fn candidates_are_distinct_models_cheapest_first() {
        let kept = dedupe_tiers(vec![
            entry(Tier::Main, "sol"),
            entry(Tier::Worker, "sol"),
            entry(Tier::Utility, "luna"),
        ]);
        assert_eq!(kept, vec![entry(Tier::Utility, "luna"), entry(Tier::Main, "sol")]);
        let kept = dedupe_tiers(vec![
            entry(Tier::Main, "sol"),
            entry(Tier::Worker, "luna"),
            entry(Tier::Utility, "luna"),
        ]);
        assert_eq!(kept, vec![entry(Tier::Worker, "luna"), entry(Tier::Main, "sol")]);
    }

    /// An even split goes to the cheaper model: a wrong-by-a-little cheap pick is
    /// the accepted loss, a wrong expensive pick is the cost the mode exists to avoid.
    #[test]
    fn a_split_answer_goes_to_the_cheaper_model() {
        let entries = dedupe_tiers(vec![
            entry(Tier::Main, "sol"),
            entry(Tier::Worker, "terra"),
            entry(Tier::Utility, "luna"),
        ]);
        let split = answer("main", &[("utility", 0.1), ("worker", 0.4), ("main", 0.5)]);
        assert_eq!(compose_tier(&split, &entries), Some(Tier::Worker));
        let sure = answer("main", &[("utility", 0.05), ("worker", 0.05), ("main", 0.9)]);
        assert_eq!(compose_tier(&sure, &entries), Some(Tier::Main));
        let cheap = answer("utility", &[("utility", 0.7), ("main", 0.3)]);
        assert_eq!(compose_tier(&cheap, &entries), Some(Tier::Utility));
    }

    /// A tier that was filtered out cannot win, and no usable answer leaves the
    /// main model on the round.
    #[test]
    fn unoffered_or_missing_answers_choose_nothing() {
        let entries = dedupe_tiers(vec![entry(Tier::Main, "sol"), entry(Tier::Utility, "luna")]);
        let worker_only = answer("worker", &[("worker", 1.0)]);
        assert_eq!(compose_tier(&worker_only, &entries), None);
        let empty = distill_workspace::jev::JevAnswerSet {
            model: "test-jev".to_owned(),
            answers: Default::default(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 0,
        };
        assert_eq!(compose_tier(&empty, &entries), None);
    }

    #[test]
    fn the_question_offers_exactly_the_candidate_tiers() {
        let questions = tier_questions(
            distill_tool_types::ExecutionMode::Ultrasave,
            &[(Tier::Utility, "u".into()), (Tier::Main, "m".into())],
        )
        .expect("question");
        assert!(questions.contains_key(TIER_QUESTION));
        assert_eq!(questions.len(), 1);
    }

    /// Normal and UltraCode keep the session's model: only the two modes whose
    /// goal is money or speed may move a round, and a Jev answer queued for them
    /// is never consumed elsewhere.
    #[tokio::test(flavor = "current_thread")]
    async fn modes_without_a_cost_goal_never_route() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for mode in [
                    distill_tool_types::ExecutionMode::Normal,
                    distill_tool_types::ExecutionMode::Ultracode,
                ] {
                    let actor = super::super::support::plain_actor().await;
                    actor.execution_mode.store(mode);
                    actor
                        .jev_effort_auto
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let mut cfg = SamplingConfig {
                        model: "main".into(),
                        ..Default::default()
                    };
                    crate::jev::set_test_decision_answers([Some(answer("utility", &[]))]);
                    assert!(!actor.jev_route_execution_tier(&mut cfg).await);
                    assert_eq!(cfg.model, "main");
                    assert_eq!(crate::jev::test_decision_answers_remaining(), 1);
                    crate::jev::clear_test_decision_answers();
                }
            })
            .await;
    }

    /// With only one distinct model nothing is asked: a decision with a single
    /// option is a wasted request.
    #[tokio::test(flavor = "current_thread")]
    async fn fewer_than_two_candidates_ask_nothing() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store(distill_tool_types::ExecutionMode::Ultrasave);
                actor
                    .jev_effort_auto
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let mut cfg = SamplingConfig {
                    model: "main".into(),
                    ..Default::default()
                };
                crate::jev::set_test_decision_answers([Some(answer("utility", &[]))]);
                assert!(!actor.jev_route_execution_tier(&mut cfg).await);
                assert_eq!(crate::jev::test_decision_answers_remaining(), 1);
                crate::jev::clear_test_decision_answers();
            })
            .await;
    }
}
