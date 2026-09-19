//! Server-side doom-loop check: wire contract types and tolerant parsers.
//!
//! The inference API reports selected generation-loop detector families on streaming `/v1/responses` requests.
//! `x-grok-doom-loop-check` selects legacy labels, while `x-grok-exact-repetition-check` selects exact-repetition labels.
//! Reports use two places:
//!
//! * a non-standard mid-stream SSE event (`response.doom_loop_check`) emitted as new triggers appear, carrying the **cumulative** trigger set:
//!   `{"type": "response.doom_loop_check", "doom_loop_check": {"triggers": ["…"]}}`
//! * a `doom_loop_check: {"triggers": ["…"]}` field on the terminal response object (`response.completed` / `response.incomplete`).
//!
//! Triggers are opaque labels.
//! The grammar is `tail_repetition:{threshold}@{channel}`, `exact_repetition:{tokens}x{copies}@{channel}`, or `low_logprob@{channel}`.
//! Presence is itself the detection signal; the set is non-empty when present.
//!
//! This module is the single home for that wire shape: if the server contract changes, only this file (and its tests) should need to change.
//! Everything here is best-effort by design: malformed payloads yield `Unknown` kinds or empty trigger sets, never an error.
//! The feature can never fail a stream.

use serde::{Deserialize, Serialize};

/// Legacy detector reporting header.
/// Its value is the tail/token-diversity detector window (decimal token count).
/// Exact-repetition labels use the independent `x-grok-exact-repetition-check` sibling header.
pub const DOOM_LOOP_CHECK_HEADER: &str = "x-grok-doom-loop-check";
/// Exact-repetition reporting sibling header.
/// The default client value is the production-safe minimum of 64 tokens.
pub const EXACT_REPETITION_CHECK_HEADER: &str = "x-grok-exact-repetition-check";
pub const DEFAULT_EXACT_REPETITION_MIN_TOKENS: usize = 64;

/// `type` of the non-standard mid-stream SSE event, also its SSE `event:` name.
/// async-openai's typed `rs::ResponseStreamEvent` does not know this variant.
/// Raw payloads carrying this name or type must be intercepted before typed deserialization.
pub const DOOM_LOOP_CHECK_EVENT_TYPE: &str = "response.doom_loop_check";

/// Byte-exact `data:` payload of a check-event frame as emitted by the server; the frame's SSE `event:` name is [`DOOM_LOOP_CHECK_EVENT_TYPE`].
/// Exported as a fixture so transport tests pin the real bytes, not a paraphrase.
pub const SAMPLE_CHECK_EVENT_DATA: &str = r#"{"sequence_number":4176,"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:4@response"]}}"#;

/// Companion fixture to [`SAMPLE_CHECK_EVENT_DATA`]: the follow-up frame from the same wire sample, carrying the grown **cumulative** trigger set.
pub const SAMPLE_CHECK_EVENT_DATA_CUMULATIVE: &str = r#"{"sequence_number":4178,"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:4@response","tail_repetition:2@response"]}}"#;

/// The resolver returns `None` when the check is disabled: absence IS the off state, so there is no separate enabled flag
/// to keep in sync. When present on `SamplerConfig`, the sampler both sends the opt-in request header and parses the
/// reported triggers. The tunables are consumed by the recovery decision logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoomLoopRecoveryPolicy {
    /// Act only on `tail_repetition:{t}@thinking` triggers with `t` at or below this value.
    /// Lower thresholds indicate tighter, more confident loops.
    #[serde(default = "default_max_threshold")]
    pub max_threshold: u32,
    /// Resample budget per turn before accepting the response as-is.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Detector window sent as the value of [`DOOM_LOOP_CHECK_HEADER`].
    #[serde(default = "default_window_tokens")]
    pub window_tokens: u32,
}

fn default_max_threshold() -> u32 {
    DoomLoopRecoveryPolicy::DEFAULT_MAX_THRESHOLD
}

fn default_max_retries() -> u32 {
    DoomLoopRecoveryPolicy::DEFAULT_MAX_RETRIES
}

fn default_window_tokens() -> u32 {
    DoomLoopRecoveryPolicy::DEFAULT_RECOVERY_WINDOW_TOKENS
}

/// Channel label of the model's thinking stream, the only channel recovery acts on (loops in visible output are the user's to judge).
pub const THINKING_CHANNEL: &str = "thinking";

impl DoomLoopRecoveryPolicy {
    /// Clamp range for `max_threshold`.
    pub const MAX_THRESHOLD_RANGE: std::ops::RangeInclusive<u32> = 2..=64;
    /// Clamp range for `max_retries`.
    pub const MAX_RETRIES_RANGE: std::ops::RangeInclusive<u32> = 0..=5;
    pub const DEFAULT_MAX_THRESHOLD: u32 = 64;
    pub const DEFAULT_MAX_RETRIES: u32 = 2;
    pub const DEFAULT_RECOVERY_WINDOW_TOKENS: u32 = 1024;
    /// Inclusive range of `window_tokens` values honored on the wire.
    pub const WINDOW_TOKENS_RANGE: std::ops::RangeInclusive<u32> = 512..=4096;

    /// Clamp a configured `max_threshold` into [`Self::MAX_THRESHOLD_RANGE`].
    pub fn clamp_max_threshold(value: u32) -> u32 {
        value.clamp(
            *Self::MAX_THRESHOLD_RANGE.start(),
            *Self::MAX_THRESHOLD_RANGE.end(),
        )
    }

    /// Clamp a configured `max_retries` into [`Self::MAX_RETRIES_RANGE`].
    pub fn clamp_max_retries(value: u32) -> u32 {
        value.clamp(
            *Self::MAX_RETRIES_RANGE.start(),
            *Self::MAX_RETRIES_RANGE.end(),
        )
    }

    /// Out-of-range values fail closed to 4096, not the range floor.
    pub fn clamp_window_tokens(value: u32) -> u32 {
        if Self::WINDOW_TOKENS_RANGE.contains(&value) {
            value
        } else {
            4096
        }
    }

    /// A signal this policy treats as a real loop worth acting on: tail repetition in the thinking channel, at or below the confidence threshold.
    /// Everything else (other channels, `low_logprob`, unknown kinds, looser thresholds) is warn-only.
    pub fn is_confident(&self, signal: &DoomLoopSignal) -> bool {
        let tight = |t: u32| t <= self.max_threshold;
        signal.channel == THINKING_CHANNEL
            && matches!(signal.kind, DoomLoopSignalKind::TailRepetition(t) if tight(t))
    }

    /// Raw labels of the confident signals in `signals`; empty when none.
    pub fn confident_triggers(&self, signals: &[DoomLoopSignal]) -> Vec<String> {
        signals
            .iter()
            .filter(|s| self.is_confident(s))
            .map(|s| s.raw.clone())
            .collect()
    }
}

impl Default for DoomLoopRecoveryPolicy {
    fn default() -> Self {
        Self {
            max_threshold: Self::DEFAULT_MAX_THRESHOLD,
            max_retries: Self::DEFAULT_MAX_RETRIES,
            window_tokens: Self::DEFAULT_RECOVERY_WINDOW_TOKENS,
        }
    }
}

/// Parsed classification of a single trigger label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DoomLoopSignalKind {
    /// `tail_repetition:{threshold}@{channel}`: a repeating tail was found at the given detector threshold.
    TailRepetition(u32),
    /// `exact_repetition:{sequence_tokens}x{repeat_count}@{channel}`: an exact token sequence repeated consecutively.
    ExactRepetition {
        sequence_tokens: u32,
        repeat_count: u32,
    },
    /// `low_logprob@{channel}`: degenerate low-entropy generation.
    LowLogprob,
    /// Any label this client version cannot classify; the unparsed kind segment is preserved verbatim.
    Unknown(String),
}

/// One doom-loop trigger reported by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoomLoopSignal {
    pub kind: DoomLoopSignalKind,
    /// Channel the loop was detected on (e.g. `thinking`, `response`).
    /// Empty when the label carries no `@channel` suffix.
    pub channel: String,
    /// The verbatim label; the stable identity used for deduplication and logging.
    pub raw: String,
}

impl DoomLoopSignal {
    /// Parse a trigger label.
    /// Never fails: any grammar mismatch yields `DoomLoopSignalKind::Unknown` with the raw label preserved.
    pub fn parse(raw: &str) -> Self {
        let (head, channel) = match raw.split_once('@') {
            Some((head, channel)) => (head, channel),
            None => (raw, ""),
        };
        let kind = match head.split_once(':') {
            Some(("tail_repetition", threshold)) => match threshold.parse::<u32>() {
                Ok(t) => DoomLoopSignalKind::TailRepetition(t),
                Err(_) => DoomLoopSignalKind::Unknown(head.to_string()),
            },
            Some(("exact_repetition", dimensions)) => dimensions
                .split_once('x')
                .and_then(|(sequence, count)| {
                    Some((sequence.parse::<u32>().ok()?, count.parse::<u32>().ok()?))
                })
                .map_or_else(
                    || DoomLoopSignalKind::Unknown(head.to_string()),
                    |(sequence_tokens, repeat_count)| DoomLoopSignalKind::ExactRepetition {
                        sequence_tokens,
                        repeat_count,
                    },
                ),
            None if head == "low_logprob" => DoomLoopSignalKind::LowLogprob,
            _ => DoomLoopSignalKind::Unknown(head.to_string()),
        };
        Self {
            kind,
            channel: channel.to_string(),
            raw: raw.to_string(),
        }
    }

    /// The tightest label among `raws`: the `tail_repetition` trigger with the LOWEST threshold (tighter repetition means stronger evidence).
    /// Falls back to the first label when none parse as `tail_repetition`.
    /// Returns raw labels only, so the result is safe to emit as telemetry.
    pub fn tightest(raws: impl IntoIterator<Item = impl AsRef<str>>) -> Option<String> {
        let mut first: Option<String> = None;
        let mut best: Option<(u32, String)> = None;
        for raw in raws {
            let raw = raw.as_ref();
            if first.is_none() {
                first = Some(raw.to_string());
            }
            if let DoomLoopSignalKind::TailRepetition(t) = Self::parse(raw).kind
                && best.as_ref().is_none_or(|(bt, _)| t < *bt)
            {
                best = Some((t, raw.to_string()));
            }
        }
        best.map(|(_, raw)| raw).or(first)
    }
}

/// Result of peeking a raw SSE `data:` payload for doom-loop content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoomLoopPeek {
    /// The payload is the non-standard `response.doom_loop_check` event.
    /// The caller must swallow it (never forward to the typed event parser); the vec is empty when the payload is malformed.
    CheckEvent(Vec<DoomLoopSignal>),
    /// The payload is an ordinary event whose `response` object carries a `doom_loop_check` field (the redundant terminal copy).
    /// Forward the event as usual after recording the signals.
    ResponseField(Vec<DoomLoopSignal>),
    /// Nothing doom-loop related; forward untouched.
    None,
}

/// Tolerantly peek a raw SSE `data:` JSON payload for doom-loop content. Cheap for the common case: payloads that don't
/// mention `doom_loop_check` return [`DoomLoopPeek::None`] without a JSON parse. Anything malformed (non-JSON, wrong
/// types, missing keys) degrades to `None` or an empty trigger vec, never an error.
pub fn peek_doom_loop(data: &str) -> DoomLoopPeek {
    if !data.contains("doom_loop_check") {
        return DoomLoopPeek::None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return DoomLoopPeek::None;
    };
    if value.get("type").and_then(|t| t.as_str()) == Some(DOOM_LOOP_CHECK_EVENT_TYPE) {
        let triggers = value.pointer("/doom_loop_check/triggers");
        return DoomLoopPeek::CheckEvent(parse_triggers(triggers));
    }
    match value.pointer("/response/doom_loop_check/triggers") {
        Some(triggers) => DoomLoopPeek::ResponseField(parse_triggers(Some(triggers))),
        None => DoomLoopPeek::None,
    }
}

/// A cheap substring precheck gates the peek so normal traffic never pays a JSON parse. The type confirmation prevents
/// swallowing a legitimate event whose content text merely quotes the event-type string. An unnamed frame with an
/// unparseable payload is NOT the check event; a real server frame always carries the name or a parseable `type` tag.
pub fn is_check_event(event_name: &str, data: &str) -> bool {
    if event_name == DOOM_LOOP_CHECK_EVENT_TYPE {
        return true;
    }
    data.contains(DOOM_LOOP_CHECK_EVENT_TYPE)
        && serde_json::from_str::<serde_json::Value>(data).is_ok_and(|v| {
            v.get("type").and_then(|t| t.as_str()) == Some(DOOM_LOOP_CHECK_EVENT_TYPE)
        })
}

/// Parse a `triggers` JSON value into signals, skipping non-string entries.
/// A missing or non-array value yields an empty vec.
fn parse_triggers(triggers: Option<&serde_json::Value>) -> Vec<DoomLoopSignal> {
    triggers
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(DoomLoopSignal::parse)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tail_repetition_label() {
        let s = DoomLoopSignal::parse("tail_repetition:8@thinking");
        assert_eq!(s.kind, DoomLoopSignalKind::TailRepetition(8));
        assert_eq!(s.channel, "thinking");
        assert_eq!(s.raw, "tail_repetition:8@thinking");
    }

    #[test]
    fn parse_exact_repetition_label() {
        let signal = DoomLoopSignal::parse("exact_repetition:42x3@thinking");
        assert_eq!(
            signal.kind,
            DoomLoopSignalKind::ExactRepetition {
                sequence_tokens: 42,
                repeat_count: 3,
            }
        );
        assert_eq!(signal.channel, "thinking");
        assert_eq!(signal.raw, "exact_repetition:42x3@thinking");
    }

    #[test]
    fn parse_low_logprob_label() {
        let s = DoomLoopSignal::parse("low_logprob@response");
        assert_eq!(s.kind, DoomLoopSignalKind::LowLogprob);
        assert_eq!(s.channel, "response");
    }

    #[test]
    fn parse_unknown_kind_preserved() {
        let s = DoomLoopSignal::parse("novel_detector:3@thinking");
        assert_eq!(
            s.kind,
            DoomLoopSignalKind::Unknown("novel_detector:3".to_string())
        );
        assert_eq!(s.channel, "thinking");
        assert_eq!(s.raw, "novel_detector:3@thinking");
    }

    #[test]
    fn parse_grammar_mismatches_are_unknown_never_error() {
        // Non-numeric threshold.
        assert!(matches!(
            DoomLoopSignal::parse("tail_repetition:huge@thinking").kind,
            DoomLoopSignalKind::Unknown(_)
        ));
        assert!(matches!(
            DoomLoopSignal::parse("exact_repetition:bad@thinking").kind,
            DoomLoopSignalKind::Unknown(_)
        ));
        // low_logprob must not carry a threshold segment.
        assert!(matches!(
            DoomLoopSignal::parse("low_logprob:3@thinking").kind,
            DoomLoopSignalKind::Unknown(_)
        ));
        // Missing channel: kind still parses, channel empty.
        let s = DoomLoopSignal::parse("tail_repetition:4");
        assert_eq!(s.kind, DoomLoopSignalKind::TailRepetition(4));
        assert_eq!(s.channel, "");
        // Empty label.
        assert!(matches!(
            DoomLoopSignal::parse("").kind,
            DoomLoopSignalKind::Unknown(_)
        ));
    }

    /// Per-field serde defaults: payloads written before a field existed (or with fields yet to exist) must keep deserializing.
    #[test]
    fn policy_deserializes_with_missing_or_extra_fields() {
        let p: DoomLoopRecoveryPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p, DoomLoopRecoveryPolicy::default());
        let p: DoomLoopRecoveryPolicy = serde_json::from_str(r#"{"max_threshold":4}"#).unwrap();
        assert_eq!(p.max_threshold, 4);
        assert_eq!(p.max_retries, DoomLoopRecoveryPolicy::DEFAULT_MAX_RETRIES);
        assert_eq!(
            p.window_tokens,
            DoomLoopRecoveryPolicy::DEFAULT_RECOVERY_WINDOW_TOKENS
        );
        let p: DoomLoopRecoveryPolicy =
            serde_json::from_str(r#"{"max_retries":1,"future_knob":true}"#).unwrap();
        assert_eq!(p.max_retries, 1);
        assert_eq!(
            p.window_tokens,
            DoomLoopRecoveryPolicy::DEFAULT_RECOVERY_WINDOW_TOKENS
        );
        let p: DoomLoopRecoveryPolicy = serde_json::from_str(r#"{"window_tokens":4096}"#).unwrap();
        assert_eq!(p.window_tokens, 4096);
        assert_eq!(
            p.max_threshold,
            DoomLoopRecoveryPolicy::DEFAULT_MAX_THRESHOLD
        );
    }

    /// Pin the server's exact wire bytes (not a paraphrase).
    /// The first frame and its cumulative follow-up both classify as check events with fully parsed labels.
    #[test]
    fn sample_wire_frames_parse_byte_exactly() {
        match peek_doom_loop(SAMPLE_CHECK_EVENT_DATA) {
            DoomLoopPeek::CheckEvent(signals) => {
                let [signal] = signals.as_slice() else {
                    panic!("expected one signal: {signals:?}");
                };
                assert_eq!(signal.kind, DoomLoopSignalKind::TailRepetition(4));
                assert_eq!(signal.channel, "response");
                assert_eq!(signal.raw, "tail_repetition:4@response");
            }
            other => panic!("expected CheckEvent, got {other:?}"),
        }
        match peek_doom_loop(SAMPLE_CHECK_EVENT_DATA_CUMULATIVE) {
            DoomLoopPeek::CheckEvent(signals) => {
                let [first, second] = signals.as_slice() else {
                    panic!("expected two signals: {signals:?}");
                };
                assert_eq!(first.raw, "tail_repetition:4@response");
                assert_eq!(second.kind, DoomLoopSignalKind::TailRepetition(2));
            }
            other => panic!("expected CheckEvent, got {other:?}"),
        }
    }

    #[test]
    fn peek_check_event_parses_cumulative_triggers() {
        let data = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking","low_logprob@thinking"]}}"#;
        match peek_doom_loop(data) {
            DoomLoopPeek::CheckEvent(signals) => {
                let [first, second] = signals.as_slice() else {
                    panic!("expected two signals: {signals:?}");
                };
                assert_eq!(first.kind, DoomLoopSignalKind::TailRepetition(8));
                assert_eq!(second.kind, DoomLoopSignalKind::LowLogprob);
            }
            other => panic!("expected CheckEvent, got {other:?}"),
        }
    }

    #[test]
    fn peek_check_event_swallowed_even_when_malformed() {
        // The event type alone must classify as CheckEvent so the caller never forwards it to the typed parser, whatever the payload
        for data in [
            r#"{"type":"response.doom_loop_check"}"#,
            r#"{"type":"response.doom_loop_check","doom_loop_check":{}}"#,
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":"oops"}}"#,
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":42}}"#,
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":[1,{"a":2}]}}"#,
            r#"{"type":"response.doom_loop_check","doom_loop_check":null,"extra":true}"#,
        ] {
            match peek_doom_loop(data) {
                DoomLoopPeek::CheckEvent(signals) => assert!(signals.is_empty(), "{data}"),
                other => panic!("expected CheckEvent for {data}, got {other:?}"),
            }
        }
    }

    #[test]
    fn peek_check_event_skips_non_string_entries() {
        let data = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":[7,"tail_repetition:8@thinking",null]}}"#;
        match peek_doom_loop(data) {
            DoomLoopPeek::CheckEvent(signals) => {
                let [signal] = signals.as_slice() else {
                    panic!("expected one signal: {signals:?}");
                };
                assert_eq!(signal.raw, "tail_repetition:8@thinking");
            }
            other => panic!("expected CheckEvent, got {other:?}"),
        }
    }

    #[test]
    fn peek_terminal_response_field() {
        let data = r#"{"type":"response.completed","response":{"id":"r1","doom_loop_check":{"triggers":["tail_repetition:16@thinking"]}}}"#;
        match peek_doom_loop(data) {
            DoomLoopPeek::ResponseField(signals) => {
                let [signal] = signals.as_slice() else {
                    panic!("expected one signal: {signals:?}");
                };
                assert_eq!(signal.kind, DoomLoopSignalKind::TailRepetition(16));
            }
            other => panic!("expected ResponseField, got {other:?}"),
        }
    }

    /// Confidence requires all three: kind `TailRepetition`, channel `thinking`, and threshold at or below `max_threshold` (boundary inclusive).
    /// Each factor is falsified independently.
    #[test]
    fn confidence_requires_kind_channel_and_threshold() {
        let policy = DoomLoopRecoveryPolicy::default();
        assert!(policy.is_confident(&DoomLoopSignal::parse("tail_repetition:8@thinking")));
        assert!(policy.is_confident(&DoomLoopSignal::parse("tail_repetition:2@thinking")));
        assert!(policy.is_confident(&DoomLoopSignal::parse("tail_repetition:32@thinking")));
        assert!(policy.is_confident(&DoomLoopSignal::parse("tail_repetition:64@thinking")));
        assert!(!policy.is_confident(&DoomLoopSignal::parse("tail_repetition:65@thinking")));
        assert!(!policy.is_confident(&DoomLoopSignal::parse("tail_repetition:2@response")));
        assert!(!policy.is_confident(&DoomLoopSignal::parse("low_logprob@thinking")));
        assert!(!policy.is_confident(&DoomLoopSignal::parse("novel_detector:2@thinking")));

        let signals = vec![
            DoomLoopSignal::parse("tail_repetition:4@response"),
            DoomLoopSignal::parse("tail_repetition:4@thinking"),
            DoomLoopSignal::parse("low_logprob@thinking"),
        ];
        assert_eq!(
            policy.confident_triggers(&signals),
            vec!["tail_repetition:4@thinking".to_string()]
        );
        assert!(policy.confident_triggers(&[]).is_empty());
    }

    /// Tightest means the lowest tail-repetition threshold; non-tail labels only win when nothing parses as tail_repetition.
    #[test]
    fn tightest_prefers_lowest_tail_repetition_threshold() {
        assert_eq!(
            DoomLoopSignal::tightest([
                "tail_repetition:64@thinking",
                "tail_repetition:4@thinking",
                "tail_repetition:16@thinking",
            ]),
            Some("tail_repetition:4@thinking".to_string())
        );
        assert_eq!(
            DoomLoopSignal::tightest(["low_logprob@thinking", "tail_repetition:8@thinking"]),
            Some("tail_repetition:8@thinking".to_string())
        );
        assert_eq!(
            DoomLoopSignal::tightest(["low_logprob@thinking", "novel:2@thinking"]),
            Some("low_logprob@thinking".to_string()),
            "no tail_repetition label: fall back to the first"
        );
        assert_eq!(DoomLoopSignal::tightest(Vec::<String>::new()), None);
    }

    #[test]
    fn is_check_event_matches_name_or_payload_type() {
        // Named frame: payload validity is irrelevant.
        assert!(is_check_event(DOOM_LOOP_CHECK_EVENT_TYPE, "not json"));
        // Unnamed frame identified by its payload `type` tag.
        assert!(is_check_event("message", SAMPLE_CHECK_EVENT_DATA));
        // A normal delta QUOTING the event-type string is not the check event: the substring precheck hits but the type confirm fails
        let quoting = r#"{"type":"response.output_text.delta","delta":"response.doom_loop_check"}"#;
        assert!(!is_check_event("response.output_text.delta", quoting));
        // An unnamed frame with an unparseable payload is forwarded, not swallowed: a real server frame carries the name or a parseable `type` tag
        assert!(!is_check_event(
            "message",
            "garbage response.doom_loop_check garbage"
        ));
    }

    #[test]
    fn peek_none_for_ordinary_and_malformed_payloads() {
        assert_eq!(
            peek_doom_loop(r#"{"type":"response.output_text.delta","delta":"hi"}"#),
            DoomLoopPeek::None
        );
        // Terminal event without the field.
        assert_eq!(
            peek_doom_loop(r#"{"type":"response.completed","response":{"id":"r1"}}"#),
            DoomLoopPeek::None
        );
        // Non-JSON mentioning the key still degrades to None.
        assert_eq!(
            peek_doom_loop("doom_loop_check garbage"),
            DoomLoopPeek::None
        );
        // The key appearing in unrelated positions is ignored.
        assert_eq!(
            peek_doom_loop(r#"{"type":"response.output_text.delta","delta":"doom_loop_check"}"#),
            DoomLoopPeek::None
        );
    }
}
