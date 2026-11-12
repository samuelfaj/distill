// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use serde::{Deserialize, Serialize};

/// One session execution policy. Modes cannot be enabled simultaneously.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ExecutionMode {
    #[default]
    Normal,
    Ultrafast,
    Ultracode,
    Ultrasave,
}

/// Relative commands are resolved against the server's current session mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModeOperation {
    #[default]
    Set,
    Toggle,
    Disable,
}

impl ExecutionMode {
    pub const ALL: [Self; 4] = [
        Self::Normal,
        Self::Ultrafast,
        Self::Ultracode,
        Self::Ultrasave,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Ultrafast => "UltraFast",
            Self::Ultracode => "UltraCode",
            Self::Ultrasave => "UltraSave",
        }
    }

    /// The serialized id (`ultrafast`), as config keys and records name the mode.
    pub fn id(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Ultrafast => "ultrafast",
            Self::Ultracode => "ultracode",
            Self::Ultrasave => "ultrasave",
        }
    }

    /// The mode a serialized id names; `None` for anything else.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.id() == id)
    }
}

/// The user's selection: a fixed mode, or Auto, where Jev picks the effective
/// `ExecutionMode` per request and may change it during execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModeChoice {
    #[default]
    Auto,
    Normal,
    Ultrafast,
    Ultracode,
    Ultrasave,
}

impl ExecutionModeChoice {
    /// The fixed mode, or `None` for Auto.
    pub fn fixed(self) -> Option<ExecutionMode> {
        match self {
            Self::Auto => None,
            Self::Normal => Some(ExecutionMode::Normal),
            Self::Ultrafast => Some(ExecutionMode::Ultrafast),
            Self::Ultracode => Some(ExecutionMode::Ultracode),
            Self::Ultrasave => Some(ExecutionMode::Ultrasave),
        }
    }

    pub fn label(self) -> &'static str {
        self.fixed().map_or("Auto", ExecutionMode::label)
    }

    /// The serialized id (`auto`, `ultrafast`).
    pub fn id(self) -> &'static str {
        self.fixed().map_or("auto", ExecutionMode::id)
    }
}

impl From<ExecutionMode> for ExecutionModeChoice {
    fn from(mode: ExecutionMode) -> Self {
        match mode {
            ExecutionMode::Normal => Self::Normal,
            ExecutionMode::Ultrafast => Self::Ultrafast,
            ExecutionMode::Ultracode => Self::Ultracode,
            ExecutionMode::Ultrasave => Self::Ultrasave,
        }
    }
}

/// The user's selection is shared by the root and every descendant. In Auto,
/// each agent chooses its own effective mode for the action it is doing.
#[derive(Debug, Default)]
pub struct AtomicExecutionMode {
    selection: Arc<AtomicU8>,
    auto_mode: AtomicU8,
}

const AUTO_BIT: u8 = 0x80;

impl AtomicExecutionMode {
    pub fn new(mode: ExecutionMode) -> Self {
        Self {
            selection: Arc::new(AtomicU8::new(mode as u8)),
            auto_mode: AtomicU8::new(mode as u8),
        }
    }

    /// Inherit live user choices without sharing Auto decisions with siblings.
    pub fn inherited(&self) -> Self {
        Self {
            selection: self.selection.clone(),
            auto_mode: AtomicU8::new(self.load() as u8),
        }
    }

    fn decode(value: u8) -> ExecutionMode {
        match value {
            0 => ExecutionMode::Normal,
            1 => ExecutionMode::Ultrafast,
            2 => ExecutionMode::Ultracode,
            3 => ExecutionMode::Ultrasave,
            _ => unreachable!("only ExecutionMode values can be stored"),
        }
    }

    /// The effective mode, whether fixed or chosen by Auto.
    pub fn load(&self) -> ExecutionMode {
        let selection = self.selection.load(Ordering::Relaxed);
        Self::decode(if selection == AUTO_BIT {
            self.auto_mode.load(Ordering::Relaxed)
        } else {
            selection
        })
    }

    pub fn is_auto(&self) -> bool {
        self.selection.load(Ordering::Relaxed) == AUTO_BIT
    }

    /// Fixes the mode and leaves Auto.
    pub fn store(&self, mode: ExecutionMode) {
        self.selection.store(mode as u8, Ordering::Relaxed);
    }

    /// Select Auto, initially keeping this effective mode.
    pub fn store_auto(&self, mode: ExecutionMode) {
        self.auto_mode.store(mode as u8, Ordering::Relaxed);
        self.selection.store(AUTO_BIT, Ordering::Relaxed);
    }

    /// Apply a decision without ever replacing a user's fixed choice.
    pub fn pick_auto(&self, mode: ExecutionMode) -> bool {
        if !self.is_auto() {
            return false;
        }
        self.auto_mode.store(mode as u8, Ordering::Relaxed);
        self.is_auto()
    }

    /// The user's selection: Auto or the fixed mode.
    pub fn choice(&self) -> ExecutionModeChoice {
        let selection = self.selection.load(Ordering::Relaxed);
        if selection == AUTO_BIT {
            ExecutionModeChoice::Auto
        } else {
            Self::decode(selection).into()
        }
    }
}

/// Probability mass at which a Jev choice over cost-ordered options is read
/// (cheapest first): the level where the cumulative mass reaches it. Read by
/// `compose_micro_effort_for` in the Jev routing catalog for effort and tier.
pub const JEV_CHOICE_QUANTILE: f64 = 0.5;

/// Default wait for one Jev answer before the harness keeps its current path
/// (`DEFAULT_ITEM_BUDGET` in the Jev client reads it; `[jev] item_budget_ms`
/// overrides it for every mode).
pub const JEV_DEADLINE_MS: u64 = 4_000;

/// A reasoning-effort level a policy names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
}

/// How often the main model's effort is decided within a prompt turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortCadence {
    /// Decided per main round; once the prompt reaches the size where an
    /// effort switch re-reads it uncached (16,384 tokens, on a backend whose
    /// cache is keyed by effort), the turn keeps the first route
    /// (`jev_keep_turn_effort`, `kept_turn_route`).
    PerRoundKeptOnLargePrompt,
    /// Decided per main round, but the turn keeps its route at any prompt
    /// size until a tool fails (`reuse_small_prompt` in
    /// `jev_keep_turn_effort`).
    PerRoundKeptUntilToolFails,
    /// One decision per prompt turn.
    PerTurn,
}

/// The effort used when Jev gives no usable answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilentEffort {
    /// The mode's baseline level, or the nearest one the model offers
    /// (`mode_fallback_effort`); it only ever lowers a pinned effort.
    Level(EffortLevel),
    /// The last choice Jev answered in this turn, else this level.
    LastAnsweredElse(EffortLevel),
}

/// Which balance the effort question asks Jev to strike
/// (`effort_execution_policy`, prose sent in the decision state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLean {
    Balanced,
    /// One level lower than balanced, for money.
    Money,
    /// Low and medium, for wall-clock time.
    Time,
    /// One level higher than balanced, for correctness.
    Correctness,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffortPolicy {
    pub cadence: EffortCadence,
    /// See [`JEV_CHOICE_QUANTILE`].
    pub quantile: f64,
    /// Highest level the harness lets a decision pick; `None` lets the model's
    /// whole menu through.
    pub ceiling: Option<EffortLevel>,
    /// Lowest level after a failed trusted check or a failed tool in the turn.
    pub floor_after_failure: Option<EffortLevel>,
    /// Whether the pass that may only lower a round's effort runs
    /// (`jev_apply_model_tier`, skipped today for UltraFast and UltraSave).
    pub lowering_pass: bool,
    pub when_silent: SilentEffort,
    pub lean: EffortLean,
    /// Whether a round whose only tool calls wait on background work, and the
    /// round that answers the final-check reminder, keep the last level
    /// instead of asking the effort question.
    pub idle_rounds_keep_level: bool,
}

/// Which of the user's three models (main, worker, utility) a main round may
/// be routed to (`tier_routing_allowed`, `tier_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierRouting {
    Off,
    Cheapest,
    Fastest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevWaitPolicy {
    /// Whether turn-start Jev questions are awaited before the turn's first
    /// main request.
    pub await_turn_start: bool,
    /// Wait for one Jev answer, in milliseconds.
    pub deadline_ms: u64,
    /// Wait for the after-edit diff-risk question (C4), in milliseconds.
    pub diff_risk_deadline_ms: u64,
}

/// `[compaction] cold_return` values a policy can force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdReturn {
    Offer,
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    /// Whether `[modes.<mode>] cold_return` and `max_prompt_tokens` replace
    /// this row's values and the `[compaction]` keys
    /// (`BoundarySettings::for_mode`; UltraSave only on the today rows).
    pub mode_keys: bool,
    /// `None` keeps the configured cold-return behaviour.
    pub cold_return: Option<ColdReturn>,
    /// Absolute prompt cap applied at a turn boundary; `None` keeps the
    /// configured `max_prompt_tokens`, `Some(0)` turns a configured cap off.
    pub max_prompt_tokens: Option<u64>,
    /// Whether file reads may be narrowed before they reach the model.
    pub narrow_reads: bool,
}

/// What checks the turn's work before the turn ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndVerification {
    None,
    /// An independent reviewer judges the turn's patch (UltraCode v2 turn
    /// review; `[modes.ultracode] contract = "v2"` also turns it on).
    IndependentReview,
}

/// How a mode responds to a failure signal within a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationLadder {
    None,
    Save,
    Fast,
}

/// How deep a session's subagents may nest (`ExecutionPolicy::effective_max_depth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentDepth {
    /// The configured depth for Normal (`off_max_depth`).
    Default,
    /// At most one level below the root.
    One,
    /// The configured UltraCode depth (`max_depth`).
    Max,
    /// No subagents: the session does the work itself. A lone child costs its
    /// start-up and the hand-back, which is all the time a quick task has.
    None,
}

/// Fields that follow the EFFECTIVE mode, so they also apply under Auto:
/// none of them changes the cached prompt prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuntimePolicy {
    pub effort: EffortPolicy,
    pub tier_routing: TierRouting,
    pub jev: JevWaitPolicy,
    /// Whether the utility model may select what of a tool result reaches the
    /// next request.
    pub utility_selection: bool,
    pub context: ContextPolicy,
    pub turn_end: TurnEndVerification,
    pub escalation: EscalationLadder,
    pub subagent_depth: SubagentDepth,
    /// Skill descriptors offered per request; `None` keeps the default limit
    /// (`MODEL_SKILL_DESCRIPTOR_LIMIT`), `Some(4)` in the lean modes today.
    pub skill_descriptor_cap: Option<u8>,
}

/// Fields that change the cached prompt prefix (tool list, directive, model,
/// provider path). They apply only to a PINNED mode; under Auto they are
/// absent (see [`ModePolicy::prefix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixPolicy {
    /// The fixed direct-work tool list (`execution_mode_tool_definitions`)
    /// instead of the harness selection.
    pub lean_tools: bool,
    /// Whose execution-mode directive the session states (`ultracode.rs`).
    pub directive: ExecutionMode,
    /// Whether `[modes.<mode>] model` may lead the session (`pinned_mode_settings`).
    pub model_key: bool,
    /// Whether `[modes.<mode>] provider_tier` picks the serving path.
    pub provider_tier_key: bool,
}

/// One row of the per-mode policy table: what the harness does for a mode.
/// `today` is what the code does now; `v2` is the target row, selected per
/// mode by `[modes] v2`. Most v2 values are declared here before any lever
/// reads them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModePolicy {
    pub runtime: RuntimePolicy,
    prefix: PrefixPolicy,
}

impl ModePolicy {
    /// What the code does today for `mode`.
    pub const fn today(mode: ExecutionMode) -> Self {
        use ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
        let lean = matches!(mode, Ultrafast | Ultrasave);
        Self {
            runtime: RuntimePolicy {
                effort: EffortPolicy {
                    cadence: if lean {
                        EffortCadence::PerRoundKeptUntilToolFails
                    } else {
                        EffortCadence::PerRoundKeptOnLargePrompt
                    },
                    quantile: JEV_CHOICE_QUANTILE,
                    ceiling: None,
                    floor_after_failure: None,
                    lowering_pass: !lean,
                    when_silent: SilentEffort::Level(match mode {
                        Ultrasave | Ultrafast => EffortLevel::Low,
                        Normal => EffortLevel::Medium,
                        Ultracode => EffortLevel::High,
                    }),
                    lean: match mode {
                        Normal => EffortLean::Balanced,
                        Ultrasave => EffortLean::Money,
                        Ultrafast => EffortLean::Time,
                        Ultracode => EffortLean::Correctness,
                    },
                    idle_rounds_keep_level: false,
                },
                tier_routing: match mode {
                    Ultrasave => TierRouting::Cheapest,
                    Ultrafast => TierRouting::Fastest,
                    Normal | Ultracode => TierRouting::Off,
                },
                jev: JevWaitPolicy {
                    await_turn_start: true,
                    deadline_ms: JEV_DEADLINE_MS,
                    diff_risk_deadline_ms: JEV_DEADLINE_MS,
                },
                utility_selection: true,
                context: ContextPolicy {
                    mode_keys: matches!(mode, Ultrasave),
                    cold_return: None,
                    max_prompt_tokens: None,
                    narrow_reads: true,
                },
                turn_end: TurnEndVerification::None,
                escalation: EscalationLadder::None,
                subagent_depth: match mode {
                    Normal => SubagentDepth::Default,
                    Ultrafast | Ultrasave => SubagentDepth::One,
                    Ultracode => SubagentDepth::Max,
                },
                skill_descriptor_cap: if lean { Some(4) } else { None },
            },
            prefix: PrefixPolicy {
                lean_tools: lean,
                directive: mode,
                model_key: lean,
                provider_tier_key: lean,
            },
        }
    }

    /// The target row for `mode`. Normal's equals its `today` row.
    pub const fn v2(mode: ExecutionMode) -> Self {
        let mut row = Self::today(mode);
        let runtime = &mut row.runtime;
        runtime.effort.idle_rounds_keep_level = !matches!(mode, ExecutionMode::Normal);
        match mode {
            ExecutionMode::Normal => {}
            ExecutionMode::Ultrafast => {
                runtime.effort.cadence = EffortCadence::PerTurn;
                runtime.effort.ceiling = Some(EffortLevel::Medium);
                runtime.jev.await_turn_start = false;
                runtime.jev.diff_risk_deadline_ms = 800;
                runtime.utility_selection = false;
                runtime.subagent_depth = SubagentDepth::None;
                // The session's own model, served fast: a cheaper tier is not
                // reliably a faster one, and it costs the result its model.
                runtime.tier_routing = TierRouting::Off;
                runtime.context.cold_return = Some(ColdReturn::Off);
                runtime.context.max_prompt_tokens = Some(0);
                runtime.context.mode_keys = true;
                runtime.escalation = EscalationLadder::Fast;
            }
            ExecutionMode::Ultracode => {
                runtime.effort.quantile = 0.75;
                runtime.effort.floor_after_failure = Some(EffortLevel::High);
                runtime.effort.lowering_pass = false;
                runtime.effort.when_silent = SilentEffort::LastAnsweredElse(EffortLevel::Medium);
                runtime.context.cold_return = Some(ColdReturn::Offer);
                runtime.context.max_prompt_tokens = Some(0);
                runtime.context.mode_keys = true;
                runtime.context.narrow_reads = false;
                runtime.turn_end = TurnEndVerification::IndependentReview;
            }
            ExecutionMode::Ultrasave => {
                runtime.effort.quantile = 0.4;
                runtime.effort.ceiling = Some(EffortLevel::High);
                runtime.context.cold_return = Some(ColdReturn::Auto);
                runtime.context.max_prompt_tokens = Some(250_000);
                runtime.escalation = EscalationLadder::Save;
            }
        }
        row
    }

    /// The row `mode` uses: `v2` when `[modes] v2` lists it, else `today`.
    pub const fn resolve(mode: ExecutionMode, v2_enabled_for_mode: bool) -> Self {
        if v2_enabled_for_mode {
            Self::v2(mode)
        } else {
            Self::today(mode)
        }
    }

    /// The prefix view for a session whose selection is `choice`: present only
    /// when the mode is pinned (a pinned session's effective mode is the
    /// pinned one), absent under Auto, where a per-mode tool list, directive,
    /// model or provider path would re-read the prompt uncached on every
    /// switch.
    pub fn prefix(&self, choice: ExecutionModeChoice) -> Option<&PrefixPolicy> {
        choice.fixed().map(|_| &self.prefix)
    }
}

/// The modes `[modes] v2` lists: each uses its `v2` row, the others `today`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V2Modes(u8);

impl V2Modes {
    pub const fn with(self, mode: ExecutionMode) -> Self {
        Self(self.0 | (1 << mode as u8))
    }

    pub const fn contains(self, mode: ExecutionMode) -> bool {
        self.0 & (1 << mode as u8) != 0
    }

    /// The row `mode` resolves to.
    pub const fn policy(self, mode: ExecutionMode) -> ModePolicy {
        ModePolicy::resolve(mode, self.contains(mode))
    }
}

impl ModePolicy {
    /// The harness-enforced fields in which two rows differ, by name.
    /// `effort.lean` and `prefix.directive` are left out: they are text that
    /// asks Jev or the model for a behaviour, not a behaviour the harness
    /// enforces. Prefix fields are named `prefix.*`; they apply to a pinned
    /// mode only. A field added to the row is added here.
    pub fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let (a, b) = (&self.runtime, &other.runtime);
        let (p, q) = (&self.prefix, &other.prefix);
        [
            ("effort.cadence", a.effort.cadence != b.effort.cadence),
            ("effort.quantile", a.effort.quantile != b.effort.quantile),
            ("effort.ceiling", a.effort.ceiling != b.effort.ceiling),
            (
                "effort.floor_after_failure",
                a.effort.floor_after_failure != b.effort.floor_after_failure,
            ),
            (
                "effort.lowering_pass",
                a.effort.lowering_pass != b.effort.lowering_pass,
            ),
            (
                "effort.when_silent",
                a.effort.when_silent != b.effort.when_silent,
            ),
            ("tier_routing", a.tier_routing != b.tier_routing),
            (
                "jev.await_turn_start",
                a.jev.await_turn_start != b.jev.await_turn_start,
            ),
            ("jev.deadline_ms", a.jev.deadline_ms != b.jev.deadline_ms),
            (
                "jev.diff_risk_deadline_ms",
                a.jev.diff_risk_deadline_ms != b.jev.diff_risk_deadline_ms,
            ),
            (
                "utility_selection",
                a.utility_selection != b.utility_selection,
            ),
            (
                "context.mode_keys",
                a.context.mode_keys != b.context.mode_keys,
            ),
            (
                "context.cold_return",
                a.context.cold_return != b.context.cold_return,
            ),
            (
                "context.max_prompt_tokens",
                a.context.max_prompt_tokens != b.context.max_prompt_tokens,
            ),
            (
                "context.narrow_reads",
                a.context.narrow_reads != b.context.narrow_reads,
            ),
            ("turn_end", a.turn_end != b.turn_end),
            ("escalation", a.escalation != b.escalation),
            ("subagent_depth", a.subagent_depth != b.subagent_depth),
            (
                "skill_descriptor_cap",
                a.skill_descriptor_cap != b.skill_descriptor_cap,
            ),
            ("prefix.lean_tools", p.lean_tools != q.lean_tools),
            ("prefix.model_key", p.model_key != q.model_key),
            (
                "prefix.provider_tier_key",
                p.provider_tier_key != q.provider_tier_key,
            ),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With `[modes] v2` empty every mode keeps exactly today's behaviour, so
    /// declaring the table changes nothing until a mode is listed.
    #[test]
    fn an_empty_v2_list_resolves_every_mode_to_today() {
        for mode in ExecutionMode::ALL {
            assert_eq!(
                V2Modes::default().policy(mode),
                ModePolicy::today(mode),
                "{mode:?}"
            );
        }
    }

    /// Listing one mode moves only that mode to its target row.
    #[test]
    fn listing_a_mode_selects_its_v2_row_and_leaves_the_others() {
        for listed in ExecutionMode::ALL {
            let v2 = V2Modes::default().with(listed);
            for mode in ExecutionMode::ALL {
                let expected = if mode == listed {
                    ModePolicy::v2(mode)
                } else {
                    ModePolicy::today(mode)
                };
                assert_eq!(
                    v2.policy(mode),
                    expected,
                    "listed {listed:?}, mode {mode:?}"
                );
            }
        }
        for mode in [
            ExecutionMode::Ultrafast,
            ExecutionMode::Ultracode,
            ExecutionMode::Ultrasave,
        ] {
            assert_ne!(
                ModePolicy::v2(mode),
                ModePolicy::today(mode),
                "{mode:?} declares a target"
            );
        }
    }

    /// Normal is the reference the Ultra modes are measured against: its
    /// target row is today's behaviour.
    #[test]
    fn normal_v2_is_normal_today() {
        assert_eq!(
            ModePolicy::v2(ExecutionMode::Normal),
            ModePolicy::today(ExecutionMode::Normal)
        );
    }

    /// Modes that differ mostly in what they ask the model are one mode with
    /// several names: every pair of v2 rows differs in at least five fields
    /// the harness enforces, and in at least five runtime fields, which also
    /// apply under Auto.
    #[test]
    fn every_pair_of_v2_rows_differs_in_at_least_five_enforced_fields() {
        const MIN_DIFFERING_FIELDS: usize = 5;
        for (index, a) in ExecutionMode::ALL.into_iter().enumerate() {
            for b in ExecutionMode::ALL.into_iter().skip(index + 1) {
                let fields = ModePolicy::v2(a).differing_fields(&ModePolicy::v2(b));
                let runtime = fields
                    .iter()
                    .filter(|name| !name.starts_with("prefix."))
                    .count();
                assert!(
                    fields.len() >= MIN_DIFFERING_FIELDS && runtime >= MIN_DIFFERING_FIELDS,
                    "{a:?} vs {b:?}: {} fields ({runtime} runtime): {fields:?}",
                    fields.len()
                );
            }
        }
        let row = ModePolicy::v2(ExecutionMode::Ultracode);
        assert!(row.differing_fields(&row).is_empty());
        assert!(
            row.differing_fields(&ModePolicy::today(ExecutionMode::Ultracode))
                .contains(&"turn_end")
        );
    }

    /// Under Auto the mode can change between rounds; a per-mode tool list,
    /// directive or model there would re-read the whole prompt uncached, so
    /// only a pinned selection gets the prefix view.
    #[test]
    fn the_prefix_view_is_absent_under_auto() {
        for mode in ExecutionMode::ALL {
            for row in [ModePolicy::today(mode), ModePolicy::v2(mode)] {
                assert_eq!(row.prefix(ExecutionModeChoice::Auto), None);
                assert_eq!(
                    row.prefix(mode.into()).map(|prefix| prefix.directive),
                    Some(mode)
                );
            }
        }
    }

    /// The table's today values are the constants the levers read, and the
    /// rows reproduce the branches cited in the field docs.
    #[test]
    fn today_rows_match_the_code() {
        let row = |mode| ModePolicy::today(mode).runtime;
        for mode in ExecutionMode::ALL {
            assert_eq!(row(mode).effort.quantile, JEV_CHOICE_QUANTILE);
            assert_eq!(row(mode).jev.deadline_ms, 4_000);
        }
        assert_eq!(
            row(ExecutionMode::Ultrasave).tier_routing,
            TierRouting::Cheapest
        );
        assert_eq!(
            row(ExecutionMode::Ultrafast).tier_routing,
            TierRouting::Fastest
        );
        assert!(!row(ExecutionMode::Ultrafast).effort.lowering_pass);
        assert!(row(ExecutionMode::Ultracode).effort.lowering_pass);
        assert_eq!(
            row(ExecutionMode::Ultracode).effort.when_silent,
            SilentEffort::Level(EffortLevel::High)
        );
        assert!(row(ExecutionMode::Ultrasave).context.mode_keys);
        assert!(!row(ExecutionMode::Ultrafast).context.mode_keys);
    }

    #[test]
    fn mode_ids_round_trip_through_serde() {
        for mode in ExecutionMode::ALL {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.id());
            assert_eq!(ExecutionMode::from_id(mode.id()), Some(mode));
            assert_eq!(
                serde_json::to_value(ExecutionModeChoice::from(mode)).unwrap(),
                ExecutionModeChoice::from(mode).id()
            );
        }
        assert_eq!(ExecutionModeChoice::Auto.id(), "auto");
        assert_eq!(ExecutionMode::from_id("auto"), None);
    }

    #[test]
    fn auto_keeps_the_effective_mode_until_a_fixed_choice_replaces_it() {
        let mode = AtomicExecutionMode::default();
        assert_eq!(mode.choice(), ExecutionModeChoice::Normal);
        mode.store_auto(ExecutionMode::Ultrasave);
        assert!(mode.is_auto());
        assert_eq!(mode.load(), ExecutionMode::Ultrasave);
        assert_eq!(mode.choice(), ExecutionModeChoice::Auto);
        mode.store(ExecutionMode::Ultracode);
        assert!(!mode.is_auto());
        assert_eq!(mode.choice(), ExecutionModeChoice::Ultracode);
        assert_eq!(
            serde_json::to_value(ExecutionModeChoice::Auto).unwrap(),
            "auto"
        );
    }

    #[test]
    fn descendants_inherit_user_choices_but_adapt_auto_independently() {
        let root = AtomicExecutionMode::default();
        root.store_auto(ExecutionMode::Ultracode);
        let child = root.inherited();
        let sibling = root.inherited();
        let grandchild = child.inherited();
        assert!(child.pick_auto(ExecutionMode::Ultrafast));
        assert!(grandchild.pick_auto(ExecutionMode::Ultrasave));
        assert_eq!(root.load(), ExecutionMode::Ultracode);
        assert_eq!(sibling.load(), ExecutionMode::Ultracode);
        assert_eq!(child.load(), ExecutionMode::Ultrafast);
        assert_eq!(grandchild.load(), ExecutionMode::Ultrasave);
        root.store(ExecutionMode::Normal);
        for agent in [&root, &child, &sibling, &grandchild] {
            assert_eq!(agent.choice(), ExecutionModeChoice::Normal);
            assert!(!agent.pick_auto(ExecutionMode::Ultracode));
            assert_eq!(agent.load(), ExecutionMode::Normal);
        }
        root.store_auto(ExecutionMode::Ultracode);
        assert_eq!(child.choice(), ExecutionModeChoice::Auto);
        assert!(child.pick_auto(ExecutionMode::Ultrafast));
        assert_eq!(root.load(), ExecutionMode::Ultracode);
    }
}
