// Modified for Distill by Samuel Fajreldines, 2026.
//! A human prompt on a large session whose prompt cache expired while it sat idle. The request
//! about to go out writes the whole history to the cache again at full price either way, so this
//! is the one moment a compaction costs no cache it would otherwise keep:
//! `[compaction] cold_return` decides whether to offer it (`"offer"`, the default), run it
//! (`"auto"`) or leave the history alone (`"off"`). An interactive session that sat idle in this
//! process counts, and so does a session reloaded past the cache lifetime its last request
//! recorded; a headless session, which cannot be asked, compacts under `"auto"` only. Unset is
//! `"offer"` over those moments (not benchmarked). Subagent sessions never do anything here, and
//! every unknown (an endpoint whose lifetime is unknown, a config that fails to load, a missing or
//! unreadable `cache_state.json`, a client that cannot ask) keeps the full history.
//!
//! The same boundary applies `[compaction] max_prompt_tokens`, an absolute cap beside the percent
//! threshold: a history above it compacts before the turn's first request, never mid-turn.
//!
//! Both follow the effective mode's policy row ([`BoundarySettings::for_mode`]): an explicit
//! `[modes.<mode>]` key wins, then the row's value, then the `[compaction]` key. On the v2 rows
//! UltraSave compacts at a cold moment without asking (from [`auto_cold_return_floor`]) and caps
//! the prompt at 250,000 tokens, UltraFast neither asks nor compacts for cost, UltraCode asks and
//! has no cap, and Normal keeps the `[compaction]` values. The today rows keep the `[compaction]`
//! values (with `[modes.ultrasave]` replacing them in UltraSave). No pass-rate evidence yet backs
//! the UltraSave values; `[modes] v2 = []` turns them off.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use distill_sampling_types::ConversationItem;
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
/// and 15 or more under 50k, where the summary's loss outweighs the saving. That assumes a 0.1x
/// cache read; at the 0.05x or 0.025x of the larger Claude models the same payback needs about
/// twice or four times the history, which `[compaction] cold_return_min_tokens` can ask for.
pub(crate) const COLD_RETURN_MIN_TOKENS: u64 = 100_000;

/// Summary size the automatic floor assumes, in tokens.
const SUMMARY_TOKENS: f64 = 10_000.0;
/// What the summary costs once, per summary token at the input price: its output at about five
/// times the input price plus its cold write at 1.25 times.
const SUMMARY_COST_FACTOR: f64 = 6.25;
/// Later requests within which an automatic compaction must pay for itself: about one returning
/// turn of tool rounds, the payback [`COLD_RETURN_MIN_TOKENS`] gives at a 0.1x cache read.
const PAYBACK_REQUESTS: f64 = 7.0;

/// The smallest history an automatic cold-return compaction pays for, at a cache read of
/// `cache_read_ratio` times the input price, never below `configured_min`. Keeping the history
/// costs `ratio × N` on each later request; compacting costs `6.25 × S` once and `ratio × S` per
/// request after it, so it pays back after `6.25 × S / (ratio × (N − S))` requests. Asking for
/// that within 7 requests gives `N = S + 6.25 × S / (7 × ratio)`: about 99k at 0.1, 189k at 0.05
/// and 367k at 0.025. It assumes a 10k-token summary, output at five times the input price, and,
/// as at any cold moment, the uncached write of `N` paid on both paths; it ignores what the
/// summary loses, which is one reason the floor never drops below the configured one.
pub(crate) fn auto_cold_return_floor(cache_read_ratio: f64, configured_min: u64) -> u64 {
    let floor = SUMMARY_TOKENS
        + SUMMARY_COST_FACTOR * SUMMARY_TOKENS / (PAYBACK_REQUESTS * cache_read_ratio);
    (floor.ceil() as u64).max(configured_min)
}

/// Decision labels counted under `compaction_timing` in usage.json, one per compaction a
/// non-default key decided.
const MAX_PROMPT_LABEL: &str = "compact:max-prompt";
const COLD_RESUME_LABEL: &str = "compact:cold-resume";
const COLD_HEADLESS_LABEL: &str = "compact:cold-headless";

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
    #[cfg(test)]
    pub(crate) fn from_setting(value: Option<&str>) -> Self {
        Self::parse(value).unwrap_or(Self::Offer)
    }

    /// The mode a value sets; `None` when unset or unrecognised, which keeps the default.
    fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim)? {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "offer" => Some(Self::Offer),
            other => {
                tracing::warn!(value = other, "unknown [compaction] cold_return; offering");
                None
            }
        }
    }

    /// The mode in a loaded config root, read the way the boundary reads it outside UltraSave.
    #[cfg(test)]
    pub(crate) fn from_config(root: &toml::Value) -> Self {
        BoundarySettings::from_config(root, false)
            .cold_return
            .unwrap_or(Self::Offer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferAnswer {
    Compact,
    Keep,
    Never,
}

/// Where the prompt cache stands as a human prompt arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColdMoment {
    /// Live, or not known to have expired.
    Warm,
    /// Idle in this process past the lifetime the last request reported.
    Idle,
    /// Reloaded past the lifetime `cache_state.json` recorded, before this process sent anything.
    Resumed,
}

/// What the boundary knows about the session.
#[derive(Clone, Copy, Debug)]
struct Boundary {
    tokens: u64,
    /// The percent-threshold compaction runs before the first request anyway.
    threshold_due: bool,
    cold: ColdMoment,
    interactive: bool,
    /// No model round has answered the human prompt yet.
    turn_start: bool,
}

/// The keys the boundary applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoundarySettings {
    /// `cold_return` when set to a known value; unset behaves as `Offer`.
    cold_return: Option<ColdReturnMode>,
    min_tokens: u64,
    max_prompt_tokens: Option<u64>,
}

impl Default for BoundarySettings {
    fn default() -> Self {
        Self {
            cold_return: None,
            min_tokens: COLD_RETURN_MIN_TOKENS,
            max_prompt_tokens: None,
        }
    }
}

impl BoundarySettings {
    /// `[compaction]` in a loaded config root, with `[modes.ultrasave]` replacing `cold_return`
    /// and `max_prompt_tokens` when `ultrasave` and set. A cap of `0` is off, so UltraSave can
    /// also turn a `[compaction]` cap off.
    fn from_config(root: &toml::Value, ultrasave: bool) -> Self {
        let compaction = crate::util::config::load_config_from_toml(root).compaction;
        let tokens = |value: Option<&toml::Value>| {
            value
                .and_then(toml::Value::as_integer)
                .and_then(|tokens| u64::try_from(tokens).ok())
        };
        let compaction_key = |key: &str| root.get("compaction")?.get(key);
        let mode_key = |key: &str| {
            ultrasave
                .then(|| root.get("modes")?.get("ultrasave")?.get(key))
                .flatten()
        };
        let cold_return = mode_key("cold_return")
            .and_then(toml::Value::as_str)
            .or(compaction.cold_return.as_deref());
        let max_prompt_tokens = tokens(mode_key("max_prompt_tokens"))
            .or_else(|| tokens(compaction_key("max_prompt_tokens")));
        Self {
            cold_return: ColdReturnMode::parse(cold_return),
            min_tokens: tokens(compaction_key("cold_return_min_tokens"))
                .unwrap_or(COLD_RETURN_MIN_TOKENS),
            max_prompt_tokens: max_prompt_tokens.filter(|cap| *cap > 0),
        }
    }
}

impl BoundarySettings {
    /// The settings while the effective mode is `mode` with the row's `context`: each of
    /// `cold_return` and `max_prompt_tokens` is the `[modes.<mode>]` key when the row reads mode
    /// keys and it is set, else the row's value, else the `[compaction]` value as
    /// [`Self::from_config`] resolves it. A cap of `0` is off at either level. When the row
    /// compacts on its own at a cold moment, the floor is [`auto_cold_return_floor`] at
    /// `cache_read_ratio`.
    fn for_mode(
        root: &toml::Value,
        mode: distill_tool_types::ExecutionMode,
        context: distill_tool_types::ContextPolicy,
        cache_read_ratio: f64,
    ) -> Self {
        use distill_tool_types::ColdReturn;
        let mut settings = Self::from_config(root, false);
        let mode_key = |key: &str| {
            context
                .mode_keys
                .then(|| root.get("modes")?.get(mode.id())?.get(key))
                .flatten()
        };
        let row_cold_return = context.cold_return.map(|value| match value {
            ColdReturn::Offer => ColdReturnMode::Offer,
            ColdReturn::Auto => ColdReturnMode::Auto,
            ColdReturn::Off => ColdReturnMode::Off,
        });
        if let Some(cold_return) =
            ColdReturnMode::parse(mode_key("cold_return").and_then(toml::Value::as_str))
                .or(row_cold_return)
        {
            settings.cold_return = Some(cold_return);
        }
        if let Some(cap) = mode_key("max_prompt_tokens")
            .and_then(toml::Value::as_integer)
            .and_then(|tokens| u64::try_from(tokens).ok())
            .or(context.max_prompt_tokens)
        {
            settings.max_prompt_tokens = (cap > 0).then_some(cap);
        }
        if row_cold_return == Some(ColdReturnMode::Auto)
            && settings.cold_return == Some(ColdReturnMode::Auto)
        {
            settings.min_tokens = auto_cold_return_floor(cache_read_ratio, settings.min_tokens);
        }
        settings
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundaryAction {
    Keep,
    /// Ask first; a yes compacts and counts the label, if any.
    Ask(Option<&'static str>),
    Compact(Option<&'static str>),
}

/// What a human prompt's boundary does before the first request. A due threshold compaction runs
/// regardless, so neither the cap nor a question adds anything then. The cap needs the turn's
/// start, so it never cuts a turn in progress. Below the floor a summary's output and cold write
/// cost more than the cache reads it saves.
fn decide(settings: BoundarySettings, at: Boundary) -> BoundaryAction {
    if at.threshold_due {
        return BoundaryAction::Keep;
    }
    if at.turn_start
        && settings
            .max_prompt_tokens
            .is_some_and(|cap| at.tokens > cap)
    {
        return BoundaryAction::Compact(Some(MAX_PROMPT_LABEL));
    }
    if at.cold == ColdMoment::Warm || at.tokens < settings.min_tokens {
        return BoundaryAction::Keep;
    }
    let todays_moment = at.cold == ColdMoment::Idle && at.interactive;
    let label = match at.cold {
        _ if todays_moment => None,
        ColdMoment::Resumed => Some(COLD_RESUME_LABEL),
        _ => Some(COLD_HEADLESS_LABEL),
    };
    match settings.cold_return.unwrap_or(ColdReturnMode::Offer) {
        ColdReturnMode::Off => BoundaryAction::Keep,
        ColdReturnMode::Auto => BoundaryAction::Compact(label),
        ColdReturnMode::Offer if at.interactive => BoundaryAction::Ask(label),
        ColdReturnMode::Offer => BoundaryAction::Keep,
    }
}

/// Whether a reloaded session's cache had expired: `cache_state.json` in `dir` records a known
/// lifetime that ran out by `now`. A missing, unreadable or lifetime-less record is not a cold
/// moment, as before the record existed.
fn reload_expired(dir: &Path, now: SystemTime) -> bool {
    crate::session::persistence::load_cache_resume_state(dir).is_some_and(|record| {
        let Some(lifetime) = record.cache_lifetime_secs.map(Duration::from_secs) else {
            return false;
        };
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_millis(record.last_request_at_ms))
            .and_then(|sent_at| now.duration_since(sent_at).ok())
            .is_some_and(|idle| idle >= lifetime)
    })
}

/// Whether no model round has answered the prompt that opened the current turn.
fn awaits_first_request(items: &[ConversationItem]) -> bool {
    let Some(start) = items.iter().rposition(|item| {
        matches!(item, ConversationItem::User(user) if user.synthetic_reason.starts_prompt_turn())
    }) else {
        return false;
    };
    !items
        .iter()
        .skip(start + 1)
        .any(|item| matches!(item, ConversationItem::Assistant(_)))
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
    /// cache for this history expired while the session sat idle and the history is large, or
    /// compact when the history is above `max_prompt_tokens` (see the module docs). Never fails
    /// the turn: a failed compaction is reported like any auto-compaction, and the request then
    /// goes out with whatever history is left.
    pub(super) async fn maybe_compact_on_cold_return(self: &Arc<Self>) {
        if self.startup_hints.is_subagent || self.compaction.is_suppressed() {
            return;
        }
        let cold = if self.chat_state_handle.cache_expired_while_idle().await {
            ColdMoment::Idle
        } else if self.reloaded_past_cache_lifetime().await {
            ColdMoment::Resumed
        } else {
            ColdMoment::Warm
        };
        let Some(config) = self.chat_state_handle.get_sampling_config().await else {
            return;
        };
        let tokens = self.chat_state_handle.get_estimated_total_tokens().await;
        let threshold_due = self
            .should_auto_compact(tokens, config.context_window)
            .is_some();
        let mode = self.execution_mode.load();
        let (settings, compaction_settings) = match crate::config::load_effective_config() {
            Ok(root) => (
                BoundarySettings::for_mode(
                    &root,
                    mode,
                    self.mode_policy().runtime.context,
                    distill_chat_state::cache_read_ratio(&config.model),
                ),
                BoundarySettings::from_config(&root, false),
            ),
            Err(error) => {
                tracing::warn!(%error, "cold return: config load failed; keeping the history");
                return;
            }
        };
        // Reading the history is only worth it when a cap would fire.
        let over_cap =
            |settings: BoundarySettings| settings.max_prompt_tokens.is_some_and(|cap| tokens > cap);
        let turn_start = (over_cap(settings) || over_cap(compaction_settings))
            && awaits_first_request(&self.chat_state_handle.get_conversation().await);
        let boundary = Boundary {
            tokens,
            threshold_due,
            cold,
            interactive: !self.attach_non_interactive.get(),
            turn_start,
        };
        let action = decide(settings, boundary);
        // The mode's row or keys decided this boundary when `[compaction]` alone would not.
        let by_mode = decide(compaction_settings, boundary) != action;
        let (compact, label) = match action {
            BoundaryAction::Keep => (false, None),
            BoundaryAction::Compact(label) => (true, label),
            BoundaryAction::Ask(label) => match self.ask_cold_return(tokens).await {
                OfferAnswer::Compact => (true, label),
                OfferAnswer::Keep => (false, None),
                OfferAnswer::Never => {
                    if let Err(error) =
                        crate::util::config::set_compaction_cold_return("off".to_owned()).await
                    {
                        tracing::warn!(%error, "cold return: persisting \"off\" failed");
                    }
                    (false, None)
                }
            },
        };
        if cold != ColdMoment::Warm || compact {
            tracing::info!(tokens, ?cold, ?action, compact, "cold return");
        }
        if !compact {
            if by_mode && action == BoundaryAction::Keep {
                crate::jev::record_policy(mode, "cold_return_skipped");
            }
            return;
        }
        let rule = if label == Some(MAX_PROMPT_LABEL) {
            "compaction:max_prompt"
        } else {
            "compaction:cold_return"
        };
        if by_mode {
            crate::jev::record_policy(mode, rule);
        }
        if let Some(label) = label {
            self.chat_state_handle
                .record_utility_outcome("compaction_timing", label, 0, 0, 0);
        }
        self.refresh_token_if_expired().await;
        let context_window = config.context_window.get();
        let trigger_info = super::compaction::AutoCompactTriggerInfo {
            tokens_used: tokens,
            context_window,
            percentage: distill_token_estimation::usage_percentage_u8(tokens, context_window),
            reason_override: (label != Some(MAX_PROMPT_LABEL))
                .then_some(COLD_RETURN_COMPACT_BANNER),
        };
        if let Err(error) = self.run_compact_only(trigger_info, false).await {
            tracing::error!(%error, "Cold-return compaction failed; sending the full history");
        }
        if by_mode {
            // Tokens before and after, in the counter's byte fields.
            let after = self.chat_state_handle.get_estimated_total_tokens().await;
            self.chat_state_handle.record_utility_outcome(
                "mode_compaction",
                &format!("{}:{rule}", mode.id()),
                0,
                tokens,
                after,
            );
        }
    }

    /// A session reloaded from disk whose last request's cache lifetime ran out before this
    /// process sent anything. Once a request completes here, or a warm reload restored that
    /// record, the in-process idle check covers it instead.
    async fn reloaded_past_cache_lifetime(&self) -> bool {
        self.chat_state_handle.cache_resume_state().await.is_none()
            && reload_expired(
                &crate::session::persistence::session_dir(&self.session_info),
                SystemTime::now(),
            )
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
    use distill_tool_types::{ExecutionMode, ModePolicy};
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

    fn at(tokens: u64, cold: ColdMoment, interactive: bool) -> Boundary {
        Boundary {
            tokens,
            threshold_due: false,
            cold,
            interactive,
            turn_start: true,
        }
    }

    fn root(text: &str) -> toml::Value {
        toml::from_str(text).expect("valid TOML")
    }

    /// Below the floor a summary's output and cold write cost more than the cache reads it saves
    /// over a turn; at the threshold the regular compaction runs anyway, so a question would only
    /// delay it.
    #[test]
    fn only_a_large_history_short_of_the_threshold_is_worth_compacting() {
        let defaults = BoundarySettings::default();
        let idle = |tokens| at(tokens, ColdMoment::Idle, true);
        assert_eq!(
            decide(defaults, idle(COLD_RETURN_MIN_TOKENS - 1)),
            BoundaryAction::Keep
        );
        assert_eq!(
            decide(defaults, idle(COLD_RETURN_MIN_TOKENS)),
            BoundaryAction::Ask(None)
        );
        let due = Boundary {
            threshold_due: true,
            ..idle(COLD_RETURN_MIN_TOKENS * 3)
        };
        assert_eq!(decide(defaults, due), BoundaryAction::Keep);
    }

    /// An empty config offers on every cold moment: every combination of size, cache state,
    /// interactivity, threshold and turn position gets the decision `cold_return = "offer"`
    /// makes, so a reloaded interactive session past its cache lifetime is asked too, and a
    /// headless session, which cannot be asked, keeps its history.
    #[test]
    fn an_empty_config_offers_like_an_explicit_offer() {
        let settings = BoundarySettings::from_config(&root(""), false);
        assert_eq!(settings, BoundarySettings::default());
        assert_eq!(
            BoundarySettings::from_config(&root(""), true),
            settings,
            "UltraSave without its keys is the same"
        );
        let offer =
            BoundarySettings::from_config(&root("[compaction]\ncold_return = \"offer\"\n"), false);
        assert_eq!(
            decide(settings, at(300_000, ColdMoment::Resumed, true)),
            BoundaryAction::Ask(Some(COLD_RESUME_LABEL)),
            "a reloaded session past its cache lifetime is asked by default"
        );
        assert_eq!(
            decide(settings, at(300_000, ColdMoment::Idle, false)),
            BoundaryAction::Keep,
            "a headless session is never compacted by default"
        );
        let today = |b: Boundary| decide(offer, b);
        for tokens in [0, 50_000, 99_999, 100_000, 450_000, 900_000] {
            for cold in [ColdMoment::Warm, ColdMoment::Idle, ColdMoment::Resumed] {
                for interactive in [true, false] {
                    for threshold_due in [true, false] {
                        for turn_start in [true, false] {
                            let b = Boundary {
                                tokens,
                                threshold_due,
                                cold,
                                interactive,
                                turn_start,
                            };
                            assert_eq!(decide(settings, b), today(b), "{b:?}");
                        }
                    }
                }
            }
        }
    }

    /// The floor moves only the size at which a cold moment is worth it, and nothing else.
    #[test]
    fn the_floor_key_moves_only_the_floor() {
        let settings = BoundarySettings::from_config(
            &root("[compaction]\ncold_return_min_tokens = 200000\n"),
            false,
        );
        assert_eq!(
            settings,
            BoundarySettings {
                min_tokens: 200_000,
                ..BoundarySettings::default()
            }
        );
        let idle = |tokens| at(tokens, ColdMoment::Idle, true);
        assert_eq!(decide(settings, idle(150_000)), BoundaryAction::Keep);
        assert_eq!(decide(settings, idle(200_000)), BoundaryAction::Ask(None));
        assert_eq!(
            decide(settings, at(150_000, ColdMoment::Resumed, true)),
            BoundaryAction::Keep,
            "the floor applies to a reloaded session too"
        );
        assert_eq!(
            decide(settings, at(500_000, ColdMoment::Resumed, false)),
            BoundaryAction::Keep,
            "the floor does not widen the moments"
        );
    }

    /// The cap compacts a large history at the start of a human turn whatever the cache state,
    /// leaves smaller ones to the other rules, never fires once a model round answered the
    /// prompt, and gives way to a due threshold compaction.
    #[test]
    fn the_cap_compacts_only_at_a_turn_start() {
        let settings = BoundarySettings::from_config(
            &root("[compaction]\nmax_prompt_tokens = 400000\n"),
            false,
        );
        assert_eq!(settings.max_prompt_tokens, Some(400_000));
        let compact = BoundaryAction::Compact(Some(MAX_PROMPT_LABEL));
        for interactive in [true, false] {
            assert_eq!(
                decide(settings, at(450_000, ColdMoment::Warm, interactive)),
                compact
            );
        }
        assert_eq!(
            decide(settings, at(450_000, ColdMoment::Idle, true)),
            compact,
            "the cap does not ask"
        );
        assert_eq!(
            decide(settings, at(400_000, ColdMoment::Warm, true)),
            BoundaryAction::Keep
        );
        let mid_turn = Boundary {
            turn_start: false,
            ..at(900_000, ColdMoment::Warm, true)
        };
        assert_eq!(decide(settings, mid_turn), BoundaryAction::Keep);
        let due = Boundary {
            threshold_due: true,
            ..at(900_000, ColdMoment::Warm, true)
        };
        assert_eq!(decide(settings, due), BoundaryAction::Keep);
        let zero =
            BoundarySettings::from_config(&root("[compaction]\nmax_prompt_tokens = 0\n"), false);
        assert_eq!(zero, BoundarySettings::default(), "0 is off");
    }

    /// The turn start is read from the history: the prompt that opened the turn with no model
    /// round after it. Reminders pushed after the prompt do not end it; an assistant reply does.
    #[test]
    fn a_turn_starts_until_the_first_model_round() {
        let opened = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("earlier"),
            ConversationItem::assistant("done"),
            ConversationItem::user("next"),
            ConversationItem::system_reminder("note"),
        ];
        assert!(awaits_first_request(&opened));
        let mut answered = opened.clone();
        answered.push(ConversationItem::assistant("working"));
        answered.push(ConversationItem::system_reminder("mid-turn note"));
        assert!(!awaits_first_request(&answered));
        assert!(!awaits_first_request(&[ConversationItem::system("sys")]));
    }

    /// `cold_return` covers a reloaded session past its cache lifetime and a headless one; a
    /// headless session cannot be asked, so only `auto` compacts it, and `off` keeps every
    /// history. An idle interactive session keeps its unlabelled decision under every value.
    #[test]
    fn each_cold_return_value_decides_every_moment() {
        let with = |value: &str| {
            BoundarySettings::from_config(
                &root(&format!("[compaction]\ncold_return = \"{value}\"\n")),
                false,
            )
        };
        let (offer, auto, off) = (with("offer"), with("auto"), with("off"));
        let resumed = at(300_000, ColdMoment::Resumed, true);
        let resumed_headless = at(300_000, ColdMoment::Resumed, false);
        let idle_headless = at(300_000, ColdMoment::Idle, false);
        let idle = at(300_000, ColdMoment::Idle, true);

        assert_eq!(
            decide(offer, resumed),
            BoundaryAction::Ask(Some(COLD_RESUME_LABEL))
        );
        assert_eq!(decide(offer, resumed_headless), BoundaryAction::Keep);
        assert_eq!(decide(offer, idle_headless), BoundaryAction::Keep);
        assert_eq!(decide(offer, idle), BoundaryAction::Ask(None));

        assert_eq!(
            decide(auto, resumed),
            BoundaryAction::Compact(Some(COLD_RESUME_LABEL))
        );
        assert_eq!(
            decide(auto, resumed_headless),
            BoundaryAction::Compact(Some(COLD_RESUME_LABEL))
        );
        assert_eq!(
            decide(auto, idle_headless),
            BoundaryAction::Compact(Some(COLD_HEADLESS_LABEL))
        );
        assert_eq!(decide(auto, idle), BoundaryAction::Compact(None));

        for b in [resumed, resumed_headless, idle_headless, idle] {
            assert_eq!(decide(off, b), BoundaryAction::Keep, "{b:?}");
        }
        assert_eq!(
            decide(auto, at(300_000, ColdMoment::Warm, true)),
            BoundaryAction::Keep,
            "a warm cache is never a cold moment"
        );
        assert_eq!(
            with("of"),
            BoundarySettings::default(),
            "a typo keeps the default, offer"
        );
    }

    /// `[modes.ultrasave]` replaces the `[compaction]` values only while the session runs in
    /// UltraSave, and an unset UltraSave key falls back to `[compaction]`.
    #[test]
    fn the_ultrasave_override_applies_only_in_ultrasave() {
        let config = root(
            "[compaction]\ncold_return = \"off\"\nmax_prompt_tokens = 600000\n\n\
             [modes.ultrasave]\ncold_return = \"auto\"\nmax_prompt_tokens = 300000\n",
        );
        let other = BoundarySettings::from_config(&config, false);
        assert_eq!(other.cold_return, Some(ColdReturnMode::Off));
        assert_eq!(other.max_prompt_tokens, Some(600_000));
        let ultrasave = BoundarySettings::from_config(&config, true);
        assert_eq!(ultrasave.cold_return, Some(ColdReturnMode::Auto));
        assert_eq!(ultrasave.max_prompt_tokens, Some(300_000));
        let b = at(450_000, ColdMoment::Warm, true);
        assert_eq!(decide(other, b), BoundaryAction::Keep);
        assert_eq!(
            decide(ultrasave, b),
            BoundaryAction::Compact(Some(MAX_PROMPT_LABEL))
        );

        let partial = root(
            "[compaction]\ncold_return = \"auto\"\nmax_prompt_tokens = 600000\n\n\
             [modes.ultrasave]\nmax_prompt_tokens = 0\n",
        );
        let ultrasave = BoundarySettings::from_config(&partial, true);
        assert_eq!(
            ultrasave.cold_return,
            Some(ColdReturnMode::Auto),
            "falls back"
        );
        assert_eq!(
            ultrasave.max_prompt_tokens, None,
            "0 turns the cap off in UltraSave"
        );
    }

    /// Settings of `mode` on its v2 (or today) row under the config `text`, at a 0.1x cache read.
    fn for_row(text: &str, mode: ExecutionMode, v2: bool) -> BoundarySettings {
        BoundarySettings::for_mode(
            &root(text),
            mode,
            ModePolicy::resolve(mode, v2).runtime.context,
            0.1,
        )
    }

    /// Every combination of size, cache state, interactivity, threshold and turn position.
    fn every_boundary() -> Vec<Boundary> {
        let mut all = Vec::new();
        for tokens in [0, 99_999, 100_000, 240_000, 250_001, 450_000, 900_000] {
            for cold in [ColdMoment::Warm, ColdMoment::Idle, ColdMoment::Resumed] {
                for interactive in [true, false] {
                    for threshold_due in [true, false] {
                        for turn_start in [true, false] {
                            all.push(Boundary {
                                tokens,
                                threshold_due,
                                cold,
                                interactive,
                                turn_start,
                            });
                        }
                    }
                }
            }
        }
        all
    }

    /// The label a cold-moment compaction carries.
    fn cold_label(b: Boundary) -> Option<&'static str> {
        match b.cold {
            ColdMoment::Idle if b.interactive => None,
            ColdMoment::Resumed => Some(COLD_RESUME_LABEL),
            _ => Some(COLD_HEADLESS_LABEL),
        }
    }

    /// A summary must pay for itself within the same number of later requests whatever the cache
    /// read costs: the floor grows as the read gets cheaper (about 2x at 0.05 and 4x at 0.025 of
    /// the 0.1 floor), keeps the configured floor at 0.1, and never goes under a configured one.
    #[test]
    fn the_automatic_floor_pays_back_at_each_cache_read_price() {
        let payback = |ratio: f64, n: u64| {
            SUMMARY_COST_FACTOR * SUMMARY_TOKENS / (ratio * (n as f64 - SUMMARY_TOKENS))
        };
        for (ratio, expected) in [(0.1, 99_286), (0.05, 188_572), (0.025, 367_143)] {
            let floor = auto_cold_return_floor(ratio, 0);
            assert_eq!(floor, expected, "{ratio}");
            assert!(payback(ratio, floor) <= PAYBACK_REQUESTS, "{ratio}");
            assert!(payback(ratio, floor - 1_000) > PAYBACK_REQUESTS, "{ratio}");
        }
        assert_eq!(
            auto_cold_return_floor(0.1, COLD_RETURN_MIN_TOKENS),
            COLD_RETURN_MIN_TOKENS,
            "at 0.1 the price floor is the configured one"
        );
        assert_eq!(auto_cold_return_floor(0.05, 400_000), 400_000);
        assert_eq!(
            for_row("", ExecutionMode::Ultrasave, true).min_tokens,
            COLD_RETURN_MIN_TOKENS
        );
        let opus = BoundarySettings::for_mode(
            &root(""),
            ExecutionMode::Ultrasave,
            ModePolicy::v2(ExecutionMode::Ultrasave).runtime.context,
            0.025,
        );
        assert_eq!(opus.min_tokens, 367_143);
        assert_eq!(
            for_row(
                "[compaction]\ncold_return = \"auto\"\n",
                ExecutionMode::Normal,
                true
            )
            .min_tokens,
            COLD_RETURN_MIN_TOKENS,
            "only a row that compacts on its own uses the price floor"
        );
    }

    /// With nothing configured, each mode's v2 row decides every boundary its own way: UltraSave
    /// compacts without asking at a cold moment and above its cap at a turn start, UltraFast never
    /// asks or compacts, UltraCode asks an interactive session and has no cap, and Normal decides
    /// what `[compaction]` decides. A due threshold compaction runs instead in every mode, and no
    /// cap ever fires once the turn has started.
    #[test]
    fn each_v2_row_decides_the_boundary_its_own_way() {
        let compaction = BoundarySettings::from_config(&root(""), false);
        for b in every_boundary() {
            let cold_and_large =
                !b.threshold_due && b.cold != ColdMoment::Warm && b.tokens >= 100_000;
            let expected = |mode| match mode {
                ExecutionMode::Normal => decide(compaction, b),
                ExecutionMode::Ultrafast => BoundaryAction::Keep,
                ExecutionMode::Ultrasave
                    if !b.threshold_due && b.turn_start && b.tokens > 250_000 =>
                {
                    BoundaryAction::Compact(Some(MAX_PROMPT_LABEL))
                }
                ExecutionMode::Ultrasave if cold_and_large => {
                    BoundaryAction::Compact(cold_label(b))
                }
                ExecutionMode::Ultracode if cold_and_large && b.interactive => {
                    BoundaryAction::Ask(cold_label(b))
                }
                _ => BoundaryAction::Keep,
            };
            for mode in ExecutionMode::ALL {
                assert_eq!(
                    decide(for_row("", mode, true), b),
                    expected(mode),
                    "{mode:?} {b:?}"
                );
            }
        }
        let capped = "[compaction]\nmax_prompt_tokens = 300000\n";
        for b in every_boundary().into_iter().filter(|b| !b.turn_start) {
            for mode in ExecutionMode::ALL {
                for v2 in [true, false] {
                    assert_ne!(
                        decide(for_row(capped, mode, v2), b),
                        BoundaryAction::Compact(Some(MAX_PROMPT_LABEL)),
                        "{mode:?} v2={v2} {b:?}"
                    );
                }
            }
        }
    }

    /// On its today row every mode resolves exactly what the boundary resolved before the rows
    /// existed: the `[compaction]` keys, with `[modes.ultrasave]` replacing them in UltraSave.
    #[test]
    fn the_today_rows_resolve_what_the_config_did() {
        for text in [
            "",
            "[compaction]\ncold_return = \"auto\"\nmax_prompt_tokens = 400000\n",
            "[compaction]\ncold_return = \"off\"\ncold_return_min_tokens = 150000\n\n\
             [modes.ultrasave]\ncold_return = \"auto\"\nmax_prompt_tokens = 300000\n\n\
             [modes.ultrafast]\ncold_return = \"offer\"\n\n\
             [modes.ultracode]\nmax_prompt_tokens = 200000\n",
        ] {
            for mode in ExecutionMode::ALL {
                assert_eq!(
                    for_row(text, mode, false),
                    BoundarySettings::from_config(&root(text), mode == ExecutionMode::Ultrasave),
                    "{mode:?} {text}"
                );
            }
        }
    }

    /// An explicit `[modes.<mode>]` key beats the row, and the row beats `[compaction]`; a key the
    /// row does not set keeps `[compaction]`, and `0` turns a cap off at either level.
    #[test]
    fn mode_keys_beat_the_row_and_the_row_beats_compaction() {
        use ColdReturnMode::{Auto, Off, Offer};
        let compaction = "[compaction]\ncold_return = \"auto\"\nmax_prompt_tokens = 400000\n\
                          cold_return_min_tokens = 120000\n";
        let resolved = |text: &str, mode, v2| {
            let settings = for_row(text, mode, v2);
            (
                settings.cold_return,
                settings.max_prompt_tokens,
                settings.min_tokens,
            )
        };
        use ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
        assert_eq!(
            resolved(compaction, Normal, true),
            (Some(Auto), Some(400_000), 120_000)
        );
        assert_eq!(
            resolved(compaction, Ultrafast, true),
            (Some(Off), None, 120_000)
        );
        assert_eq!(
            resolved(compaction, Ultracode, true),
            (Some(Offer), None, 120_000)
        );
        assert_eq!(
            resolved(compaction, Ultrasave, true),
            (Some(Auto), Some(250_000), 120_000)
        );
        assert_eq!(resolved("", Normal, true), resolved("", Normal, false));

        let keyed = format!(
            "{compaction}\n[modes.ultrafast]\ncold_return = \"offer\"\nmax_prompt_tokens = 500000\n\n\
             [modes.ultracode]\ncold_return = \"off\"\n\n\
             [modes.ultrasave]\ncold_return = \"offer\"\nmax_prompt_tokens = 0\n\n\
             [modes.normal]\ncold_return = \"off\"\n"
        );
        assert_eq!(
            resolved(&keyed, Ultrafast, true),
            (Some(Offer), Some(500_000), 120_000)
        );
        assert_eq!(
            resolved(&keyed, Ultracode, true),
            (Some(Off), None, 120_000)
        );
        assert_eq!(
            resolved(&keyed, Ultrasave, true),
            (Some(Offer), None, 120_000)
        );
        assert_eq!(
            resolved(&keyed, Normal, true),
            (Some(Auto), Some(400_000), 120_000),
            "Normal reads no mode keys"
        );
        assert_eq!(
            resolved(&keyed, Ultrafast, false),
            (Some(Auto), Some(400_000), 120_000),
            "the today row reads no [modes.ultrafast] keys"
        );
        let typo = "[modes.ultrafast]\ncold_return = \"of\"\n";
        assert_eq!(
            resolved(typo, Ultrafast, true).0,
            Some(Off),
            "a typo falls back to the row"
        );
    }

    /// UltraFast never stops a turn to ask, nor compacts to save money, whatever `[compaction]`
    /// says; only the percent threshold, which is about fitting the window, still compacts it.
    #[test]
    fn ultrafast_v2_never_asks_nor_compacts_for_cost() {
        for text in [
            "",
            "[compaction]\ncold_return = \"offer\"\n",
            "[compaction]\ncold_return = \"auto\"\nmax_prompt_tokens = 200000\n",
        ] {
            let ultrafast = for_row(text, ExecutionMode::Ultrafast, true);
            let compaction = BoundarySettings::from_config(&root(text), false);
            let mut suppressed = 0;
            for b in every_boundary() {
                assert_eq!(decide(ultrafast, b), BoundaryAction::Keep, "{text} {b:?}");
                suppressed += usize::from(decide(compaction, b) != BoundaryAction::Keep);
            }
            assert!(
                suppressed > 0,
                "the config alone would have asked or compacted: {text}"
            );
        }
    }

    /// A reload is a cold moment only when `cache_state.json` records a known lifetime that ran
    /// out; a missing or torn file, or an unknown lifetime, keeps today's behaviour.
    #[test]
    fn only_a_readable_expired_record_makes_a_reload_cold() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(crate::session::persistence::CACHE_STATE_FILE);
        let sent_ms = 1_800_000_000_000;
        let now = |idle_secs| {
            SystemTime::UNIX_EPOCH + Duration::from_millis(sent_ms) + Duration::from_secs(idle_secs)
        };
        assert!(!reload_expired(dir.path(), now(3_600)), "missing file");

        let write = |lifetime: Option<u64>| {
            let record = distill_chat_state::CacheResumeState {
                last_request_at_ms: sent_ms,
                cache_lifetime_secs: lifetime,
                request_trims: Vec::new(),
            };
            std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();
        };
        write(Some(300));
        assert!(!reload_expired(dir.path(), now(299)), "still warm");
        assert!(reload_expired(dir.path(), now(300)));
        write(None);
        assert!(!reload_expired(dir.path(), now(86_400)), "unknown lifetime");

        std::fs::write(&file, b"{\"lastRequestAtMs\":").unwrap();
        assert!(!reload_expired(dir.path(), now(86_400)), "torn file");
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
