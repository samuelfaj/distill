// Modified for Distill by Samuel Fajreldines, 2026.
//! A human prompt on a large session whose prompt cache expired while it sat idle. The request
//! about to go out writes the whole history to the cache again at full price either way, so this
//! is the one moment a compaction costs no cache it would otherwise keep:
//! `[compaction] cold_return` decides whether to offer it (`"offer"`, the default), run it
//! (`"auto"`) or leave the history alone (`"off"`). Headless and subagent sessions never do
//! anything here, and every unknown (an endpoint whose lifetime is unknown, a config that fails
//! to load, a client that cannot ask) keeps the full history, as before.

use std::sync::Arc;
use std::time::Duration;

use distill_tools::implementations::distill::ask_user_question::{
    Question, QuestionOption, UserQuestionRequest, UserQuestionResponse,
};

use crate::extensions::notification::COLD_RETURN_COMPACT_BANNER;
use crate::session::acp_session::SessionActor;

/// Smallest history worth compacting on a cold return. Keeping it costs a cache read of the whole
/// history, `0.1 × N`, on every later request; compacting costs the summary's output (about five
/// times the input price) and the cold write of the summary `S`, about `5 × S + 1.25 × S`, while the
/// cold write of `N` is paid on both paths. With `S` around 10k that pays back after
/// `62k / (0.1 × (N − S))` requests: 7 at 100k, which one returning turn of tool rounds passes,
/// and 15 or more under 50k, where the summary's loss outweighs the saving.
pub(crate) const COLD_RETURN_MIN_TOKENS: u64 = 100_000;

/// How long the offer waits for an answer before it keeps the full history: a background
/// session's question parks until a client attaches, and the turn must not hang on it.
const ANSWER_WAIT: Duration = Duration::from_secs(120);

const COMPACT_LABEL: &str = "Compact and continue";
const KEEP_LABEL: &str = "Keep full history";
const NEVER_LABEL: &str = "Don't ask again";

/// `[compaction] cold_return`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColdReturnMode {
    Offer,
    Auto,
    Off,
}

impl ColdReturnMode {
    /// The configured mode; unset or unrecognised is the default, `Offer`.
    pub(crate) fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("auto") => Self::Auto,
            Some("off") => Self::Off,
            None | Some("offer") => Self::Offer,
            Some(other) => {
                tracing::warn!(value = other, "unknown [compaction] cold_return; offering");
                Self::Offer
            }
        }
    }

    /// The mode in a loaded config root.
    pub(crate) fn from_config(root: &toml::Value) -> Self {
        Self::from_setting(
            crate::util::config::load_config_from_toml(root)
                .compaction
                .cold_return
                .as_deref(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferAnswer {
    Compact,
    Keep,
    Never,
}

/// Whether a history of `tokens`, cold anyway, is worth compacting now. When the threshold
/// compaction is due before the first request, it runs regardless, so asking would be noise.
fn worth_compacting(tokens: u64, threshold_compaction_due: bool) -> bool {
    tokens >= COLD_RETURN_MIN_TOKENS && !threshold_compaction_due
}

fn offer_question(tokens: u64) -> Question {
    let thousands = tokens / 1_000;
    let option = |label: &str, description: String| QuestionOption {
        label: label.to_owned(),
        description,
        preview: None,
        id: None,
    };
    Question {
        question: format!(
            "The prompt cache expired while this session was idle, so this message re-sends \
             all ~{thousands}k tokens of history at full price. Compact it first?"
        ),
        options: vec![
            option(
                COMPACT_LABEL,
                format!(
                    "Summarize the history, then answer. Later requests read the summary \
                     instead of ~{thousands}k tokens."
                ),
            ),
            option(KEEP_LABEL, "Send the full history this time.".to_owned()),
            option(
                NEVER_LABEL,
                "Keep the full history now and stop asking ([compaction] cold_return = \"off\"; \
                 /compact still works)."
                    .to_owned(),
            ),
        ],
        multi_select: Some(false),
        id: None,
    }
}

/// The picked option; anything but the two other labels (a cancel, a typed answer, a plan-mode
/// action) keeps the full history.
fn answer_of(response: &UserQuestionResponse) -> OfferAnswer {
    let UserQuestionResponse::Accepted { answers, .. } = response else {
        return OfferAnswer::Keep;
    };
    let picked = answers.values().flatten().map(String::as_str);
    let mut answer = OfferAnswer::Keep;
    for label in picked {
        match label {
            COMPACT_LABEL => answer = OfferAnswer::Compact,
            NEVER_LABEL => return OfferAnswer::Never,
            _ => {}
        }
    }
    answer
}

impl SessionActor {
    /// Before a human prompt's first request: offer (or run) a compaction when the provider's
    /// cache for this history expired while the session sat idle and the history is large.
    /// Never fails the turn: a failed compaction is reported like any auto-compaction, and the
    /// request then goes out with whatever history is left.
    pub(super) async fn maybe_compact_on_cold_return(self: &Arc<Self>) {
        if self.startup_hints.is_subagent
            || self.attach_non_interactive.get()
            || self.compaction.is_suppressed()
            || !self.chat_state_handle.cache_expired_while_idle().await
        {
            return;
        }
        let Some(config) = self.chat_state_handle.get_sampling_config().await else {
            return;
        };
        let tokens = self.chat_state_handle.get_estimated_total_tokens().await;
        let threshold_due = self
            .should_auto_compact(tokens, config.context_window)
            .is_some();
        if !worth_compacting(tokens, threshold_due) {
            return;
        }
        let mode = match crate::config::load_effective_config() {
            Ok(root) => ColdReturnMode::from_config(&root),
            Err(error) => {
                tracing::warn!(%error, "cold return: config load failed; keeping the history");
                ColdReturnMode::Off
            }
        };
        let compact = match mode {
            ColdReturnMode::Off => false,
            ColdReturnMode::Auto => true,
            ColdReturnMode::Offer => match self.ask_cold_return(tokens).await {
                OfferAnswer::Compact => true,
                OfferAnswer::Keep => false,
                OfferAnswer::Never => {
                    if let Err(error) =
                        crate::util::config::set_compaction_cold_return("off".to_owned()).await
                    {
                        tracing::warn!(%error, "cold return: persisting \"off\" failed");
                    }
                    false
                }
            },
        };
        tracing::info!(tokens, ?mode, compact, "cold return");
        if !compact {
            return;
        }
        self.refresh_token_if_expired().await;
        let context_window = config.context_window.get();
        let trigger_info = super::compaction::AutoCompactTriggerInfo {
            tokens_used: tokens,
            context_window,
            percentage: distill_token_estimation::usage_percentage_u8(tokens, context_window),
            reason_override: Some(COLD_RETURN_COMPACT_BANNER),
        };
        if let Err(error) = self.run_compact_only(trigger_info, false).await {
            tracing::error!(%error, "Cold-return compaction failed; sending the full history");
        }
    }

    /// Ask through the client's question UI. A client that cannot ask (`--no-ask-user`, no
    /// question support, a closed channel) or no answer within [`ANSWER_WAIT`] keeps the history.
    async fn ask_cold_return(&self, tokens: u64) -> OfferAnswer {
        if !self.rebuild_spec.ask_user_question_enabled {
            return OfferAnswer::Keep;
        }
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = UserQuestionRequest {
            tool_call_id: format!("cold-return-{}", uuid::Uuid::new_v4()),
            questions: vec![offer_question(tokens)],
            result_tx,
        };
        if self.rebuild_spec.user_question_tx.send(request).is_err() {
            return OfferAnswer::Keep;
        }
        match tokio::time::timeout(ANSWER_WAIT, result_rx).await {
            Ok(Ok(Ok(response))) => answer_of(&response),
            Ok(Ok(Err(error))) => {
                tracing::info!(?error, "cold return: the client could not ask");
                OfferAnswer::Keep
            }
            Ok(Err(_)) | Err(_) => OfferAnswer::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn accepted(labels: &[&str]) -> UserQuestionResponse {
        let mut answers = IndexMap::new();
        answers.insert(
            offer_question(150_000).question,
            labels.iter().map(|label| (*label).to_owned()).collect(),
        );
        UserQuestionResponse::Accepted {
            answers,
            annotations: None,
        }
    }

    /// The default must ask: a user who never configured anything returns to a large session and
    /// would otherwise re-pay the whole history on every request of the turn without knowing it.
    /// A typo must not silently switch the offer off or start compacting on its own either.
    #[test]
    fn the_mode_defaults_to_offer_and_reads_auto_and_off() {
        assert_eq!(ColdReturnMode::from_setting(None), ColdReturnMode::Offer);
        assert_eq!(
            ColdReturnMode::from_setting(Some("offer")),
            ColdReturnMode::Offer
        );
        assert_eq!(
            ColdReturnMode::from_setting(Some("auto")),
            ColdReturnMode::Auto
        );
        assert_eq!(
            ColdReturnMode::from_setting(Some(" off ")),
            ColdReturnMode::Off
        );
        assert_eq!(
            ColdReturnMode::from_setting(Some("of")),
            ColdReturnMode::Offer
        );
    }

    /// "Don't ask again" writes `[compaction] cold_return = "off"`, beside the pruning settings;
    /// the offer must read that section, or the answer would keep asking.
    #[test]
    fn the_mode_is_read_from_the_compaction_section() {
        let root: toml::Value = toml::from_str(
            "[compaction]\ncold_return = \"off\"\n\n[compaction.pruning]\nkeep_last_n_turns = 7\n",
        )
        .expect("valid TOML");
        assert_eq!(ColdReturnMode::from_config(&root), ColdReturnMode::Off);
        let empty: toml::Value = toml::from_str("").expect("valid TOML");
        assert_eq!(ColdReturnMode::from_config(&empty), ColdReturnMode::Offer);
    }

    /// Below the floor a summary's output and cold write cost more than the cache reads it saves
    /// over a turn; at the threshold the regular compaction runs anyway, so a question would only
    /// delay it.
    #[test]
    fn only_a_large_history_short_of_the_threshold_is_worth_compacting() {
        assert!(!worth_compacting(COLD_RETURN_MIN_TOKENS - 1, false));
        assert!(worth_compacting(COLD_RETURN_MIN_TOKENS, false));
        assert!(!worth_compacting(COLD_RETURN_MIN_TOKENS * 3, true));
    }

    /// Only an explicit pick compacts or stops the offer: dismissing the question, typing another
    /// answer or a plan-mode action keeps today's behaviour, the full history.
    #[test]
    fn only_an_explicit_pick_compacts_or_silences() {
        assert_eq!(answer_of(&accepted(&[COMPACT_LABEL])), OfferAnswer::Compact);
        assert_eq!(answer_of(&accepted(&[NEVER_LABEL])), OfferAnswer::Never);
        assert_eq!(answer_of(&accepted(&[KEEP_LABEL])), OfferAnswer::Keep);
        assert_eq!(answer_of(&accepted(&["Other"])), OfferAnswer::Keep);
        assert_eq!(
            answer_of(&UserQuestionResponse::Cancelled),
            OfferAnswer::Keep
        );
    }

    /// The three choices the user sees are the three answers [`answer_of`] knows, so a renamed
    /// label cannot quietly turn every pick into "keep".
    #[test]
    fn the_question_offers_exactly_the_known_answers() {
        let question = offer_question(180_000);
        let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, [COMPACT_LABEL, KEEP_LABEL, NEVER_LABEL]);
        assert!(question.question.contains("~180k tokens"));
        assert_eq!(question.multi_select, Some(false));
    }
}
