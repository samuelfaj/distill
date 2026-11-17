// Modified for Distill by Samuel Fajreldines, 2026.
//! Step battery: factual questions, policy in code.
//!
//! The decision model answers what it can observe: what kind of step a model
//! call is and whether a wrong step would be expensive (B2), what shape of
//! work a request needs (B6), and what kind of assignment a read-only child
//! gets (B7). The tables below turn those answers into an effort level, a tier,
//! a delegation hint or a child model. Before this battery the effort question
//! asked the decision model to weigh the mode's policy prose and pick a level
//! itself; on the labelled set in `scripts/benchmarks/jev-decisions` the
//! battery scored 32/32 on Jev 1.14 and d1.2-ultra and 30/32 on d1.2, against
//! 24/32, 31/32 and 22/32 for the old question, and it lets one answer serve
//! both the effort and the tier of a round. The question texts are copied in
//! `run.py`; keep them in sync.

use std::collections::BTreeMap;

use distill_tool_types::ExecutionMode;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Json, Question, QuestionId};

use super::routing::DELEGATION_PARALLEL_QUESTION;

// ---------------------------------------------------------------------------
// B2 — what kind of step a model call is
// ---------------------------------------------------------------------------

/// Question id of the step kind.
pub const STEP_KIND_QUESTION: &str = "step_kind";
/// Question id of "a wrong step here is expensive".
pub const WRONG_STEP_COSTLY_QUESTION: &str = "wrong_step_costly";
/// Probability at which a costly step gets one level (or one tier) more.
pub const COSTLY_FLOOR: f64 = 0.6;

/// What one model call does. The order is the order of the policy tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Lookup,
    Relay,
    EditSpecified,
    Implement,
    Diagnose,
    Design,
    Review,
}

impl StepKind {
    pub const ALL: [Self; 7] = [
        Self::Lookup,
        Self::Relay,
        Self::EditSpecified,
        Self::Implement,
        Self::Diagnose,
        Self::Design,
        Self::Review,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::Relay => "relay",
            Self::EditSpecified => "edit_specified",
            Self::Implement => "implement",
            Self::Diagnose => "diagnose",
            Self::Design => "design",
            Self::Review => "review",
        }
    }

    const fn criterion(self) -> &'static str {
        match self {
            Self::Lookup => {
                "Read, search, list or fetch something, or run a known command such as a build, a test or git, before any judgment about the result"
            }
            Self::Relay => "Report, summarize or answer from results and facts already in hand",
            Self::EditSpecified => {
                "Make edits whose exact content is already decided, even across several files: a typo, a rename, a value, a step of a plan that states the change"
            }
            Self::Implement => {
                "Write or change code from clear requirements, where the code itself still has to be worked out"
            }
            Self::Diagnose => {
                "Find the cause of a failure or of unexpected behaviour whose cause is not known yet"
            }
            Self::Design => {
                "Choose between approaches or settle unclear requirements: the request leaves open how to do it"
            }
            Self::Review => {
                "Judge subtle correctness: concurrency, security, data loss, public contracts, a proof or exact arithmetic"
            }
        }
    }

    /// Kinds where an expensive mistake earns more thinking. Reading and
    /// relaying do not get better with more effort.
    const fn bumpable(self) -> bool {
        !matches!(self, Self::Lookup | Self::Relay)
    }
}

/// The step battery: one `choice` and one `noul`, asked together.
pub fn step_battery_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let criteria: BTreeMap<String, Json> = StepKind::ALL
        .iter()
        .map(|kind| (kind.id().to_owned(), Json::from(kind.criterion())))
        .collect();
    let mut questions = BTreeMap::new();
    questions.insert(
        STEP_KIND_QUESTION.to_owned(),
        Question::choice(
            "What kind of step is the single model call described in `micro_action`? When `micro_action.step` is \
             `first_step`, that call reads the request and plans how to do all of it before any tool call: judge the \
             request itself. Otherwise judge that one step, not the whole request: a hard request still has easy steps \
             (reading a file, running a test) and an easy request can have a hard one. `what_it_must_do` is the model's \
             own last plan; `last_calls` and `last_results` are what it just did.",
            criteria,
        )?,
    );
    questions.insert(
        WRONG_STEP_COSTLY_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "If this one step is done wrong, would the mistake be expensive: lost or corrupted data, a broken public \
             contract, a wrong design that later steps build on, or a failure that will not show up in the next check? \
             A mistake the next call or check would catch and fix cheaply is not expensive.",
            "A wrong step here is expensive to notice or undo",
            "The next call or check would catch and fix it cheaply",
        ),
    );
    Ok(questions)
}

/// Canonical effort ranks, cheapest first (the shell's `effort_rank`).
pub mod rank {
    pub const NONE: u8 = 0;
    pub const MINIMAL: u8 = 1;
    pub const LOW: u8 = 2;
    pub const MEDIUM: u8 = 3;
    pub const HIGH: u8 = 4;
    pub const XHIGH: u8 = 5;
    pub const MAX: u8 = 6;
}

/// The effort a kind of step runs at in a mode. `None` means the cheapest
/// level the model offers, whatever it is called.
///
/// | kind | UltraSave | Normal | UltraFast | UltraCode |
/// |---|---|---|---|---|
/// | lookup | cheapest | cheapest | cheapest | cheapest |
/// | relay | low | low | low | low |
/// | edit_specified | low | low | low | low |
/// | implement | low | medium | medium | high |
/// | diagnose | medium | high | medium | high |
/// | design | medium | high | medium | xhigh |
/// | review | medium | high | medium | xhigh |
///
/// This is the mode prose the old effort question carried, written down:
/// UltraSave one level under Normal above relaying, UltraFast medium for
/// anything that thinks, UltraCode one level over Normal for planning, design
/// and review, and every mode cheap for mechanical steps.
pub const fn effort_target(mode: ExecutionMode, kind: StepKind) -> Option<u8> {
    use ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
    use StepKind::*;
    match (kind, mode) {
        (Lookup, _) => None,
        (Relay | EditSpecified, _) => Some(rank::LOW),
        (Implement, Ultrasave) => Some(rank::LOW),
        (Implement, Normal | Ultrafast) => Some(rank::MEDIUM),
        (Implement, Ultracode) => Some(rank::HIGH),
        (Diagnose | Design | Review, Ultrasave | Ultrafast) => Some(rank::MEDIUM),
        (Diagnose | Design | Review, Normal) => Some(rank::HIGH),
        (Diagnose, Ultracode) => Some(rank::HIGH),
        (Design | Review, Ultracode) => Some(rank::XHIGH),
    }
}

/// How far a costly step may climb: high, or xhigh in UltraCode. Levels
/// above that stay reserved for a redo or the escalation ratchet.
pub const fn costly_ceiling(mode: ExecutionMode) -> u8 {
    match mode {
        ExecutionMode::Ultracode => rank::XHIGH,
        _ => rank::HIGH,
    }
}

/// One level of the model's menu, with its canonical rank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedLevel {
    pub id: String,
    pub rank: u8,
}

/// What the battery decided for one call, for the decision record.
#[derive(Debug, Clone, PartialEq)]
pub struct StepPick {
    /// The menu id for an effort pick; the tier id for a tier pick.
    pub id: String,
    pub kind: StepKind,
    /// The costly bump was applied.
    pub costly: bool,
}

/// The kind distribution of an answer set, over known kinds only. Without
/// probabilities the plain `choice` counts as certain.
fn kind_mass(answers: &JevAnswerSet) -> Option<Vec<(StepKind, f64)>> {
    let Some(Answer::Choice {
        choice,
        probabilities,
        ..
    }) = answers.answers.get(STEP_KIND_QUESTION)
    else {
        return None;
    };
    let mut mass: Vec<(StepKind, f64)> = StepKind::ALL
        .iter()
        .map(|kind| {
            let p = probabilities.get(kind.id()).copied().unwrap_or(0.0);
            (*kind, if p.is_finite() { p.max(0.0) } else { 0.0 })
        })
        .collect();
    if mass.iter().map(|(_, p)| p).sum::<f64>() <= 0.0 {
        let kind = StepKind::ALL.iter().find(|kind| kind.id() == choice)?;
        mass = vec![(*kind, 1.0)];
    }
    Some(mass)
}

/// Reads `mass` cheapest target first and returns the kind where the running
/// share reaches `quantile`, so a split answer lands between its readings
/// (the mode row's quantile leans it lower or higher).
fn kind_at_quantile(mut mass: Vec<(StepKind, f64, u8)>, quantile: f64) -> Option<(StepKind, u8)> {
    let total: f64 = mass.iter().map(|(_, p, _)| p).sum();
    if total <= 0.0 {
        return None;
    }
    mass.sort_by_key(|(kind, _, target)| {
        (
            *target,
            StepKind::ALL.iter().position(|k| k == kind).unwrap_or(0),
        )
    });
    let mut cumulative = 0.0;
    for (kind, p, target) in &mass {
        cumulative += p;
        if cumulative / total >= quantile {
            return Some((*kind, *target));
        }
    }
    mass.last().map(|(kind, _, target)| (*kind, *target))
}

/// Whether the costly bump applies: a confident "expensive" and most of the
/// mass on kinds where thinking more helps.
fn costly(answers: &JevAnswerSet, mass: &[(StepKind, f64)]) -> bool {
    let total: f64 = mass.iter().map(|(_, p)| p).sum();
    let bumpable: f64 = mass
        .iter()
        .filter(|(kind, _)| kind.bumpable())
        .map(|(_, p)| p)
        .sum();
    let noul = match answers.answers.get(WRONG_STEP_COSTLY_QUESTION) {
        Some(Answer::Noul { noul }) => *noul,
        _ => return false,
    };
    total > 0.0 && noul >= COSTLY_FLOOR && bumpable / total >= 0.5
}

/// The effort for one call from the step battery. `offered` is the model's
/// menu, cheapest first; the pick is the offered level nearest the target,
/// ties to the cheaper one. `None` when the battery has no usable answer.
pub fn compose_step_effort(
    answers: &JevAnswerSet,
    offered: &[OfferedLevel],
    mode: ExecutionMode,
    quantile: f64,
) -> Option<StepPick> {
    let cheapest = offered.iter().map(|level| level.rank).min()?;
    let mass = kind_mass(answers)?;
    let targeted = mass
        .iter()
        .map(|(kind, p)| (*kind, *p, effort_target(mode, *kind).unwrap_or(cheapest)))
        .collect();
    let (kind, mut target) = kind_at_quantile(targeted, quantile)?;
    let bumped = costly(answers, &mass) && target < costly_ceiling(mode);
    if bumped {
        target = (target + 1).min(costly_ceiling(mode));
    }
    let level = offered
        .iter()
        .min_by_key(|level| (level.rank.abs_diff(target), level.rank))?;
    Some(StepPick {
        id: level.id.clone(),
        kind,
        costly: bumped,
    })
}

/// Tier ranks, cheapest first: the user's utility, worker and main models.
pub mod tier {
    pub const UTILITY: u8 = 0;
    pub const WORKER: u8 = 1;
    pub const MAIN: u8 = 2;
}

/// Which of the user's models a kind of step needs: mechanical steps go to
/// the cheapest, code from clear requirements to the worker, and diagnosis,
/// design and review stay on the main model. The same in UltraSave and
/// UltraFast, the two modes that route tiers; the smaller model is also the
/// faster one.
pub const fn tier_target(kind: StepKind) -> u8 {
    match kind {
        StepKind::Lookup | StepKind::Relay | StepKind::EditSpecified => tier::UTILITY,
        StepKind::Implement => tier::WORKER,
        StepKind::Diagnose | StepKind::Design | StepKind::Review => tier::MAIN,
    }
}

/// The tier for one call from the same battery answer. `offered` holds the
/// tier ranks the round may use; the pick is the cheapest offered tier at or
/// above the target, so a missing utility hands its steps to the worker. A
/// costly step moves one tier up.
pub fn compose_step_tier(
    answers: &JevAnswerSet,
    offered: &[u8],
    quantile: f64,
) -> Option<(u8, StepPick)> {
    let mass = kind_mass(answers)?;
    let targeted = mass
        .iter()
        .map(|(kind, p)| (*kind, *p, tier_target(*kind)))
        .collect();
    let (kind, mut target) = kind_at_quantile(targeted, quantile)?;
    let bumped = costly(answers, &mass) && target < tier::MAIN;
    if bumped {
        target += 1;
    }
    let picked = offered
        .iter()
        .copied()
        .filter(|rank| *rank >= target)
        .min()
        .or_else(|| offered.iter().copied().max())?;
    Some((
        picked,
        StepPick {
            id: match picked {
                tier::UTILITY => "utility",
                tier::WORKER => "worker",
                _ => "main",
            }
            .to_owned(),
            kind,
            costly: bumped,
        },
    ))
}

// ---------------------------------------------------------------------------
// B6 — what shape of work a request needs
// ---------------------------------------------------------------------------

/// Question id of the work shape.
pub const WORK_SHAPE_QUESTION: &str = "work_shape";
/// Share of the multi-step shapes needed before any delegation hint.
pub const MULTI_STEP_FLOOR: f64 = 0.6;
/// `needs_parallel` probability that asks for launching parts together.
pub const PARALLEL_FLOOR: f64 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkShape {
    DirectAnswer,
    SingleAction,
    SpecifiedWork,
    Investigation,
    OpenDesign,
}

impl WorkShape {
    pub const ALL: [Self; 5] = [
        Self::DirectAnswer,
        Self::SingleAction,
        Self::SpecifiedWork,
        Self::Investigation,
        Self::OpenDesign,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::DirectAnswer => "direct_answer",
            Self::SingleAction => "single_action",
            Self::SpecifiedWork => "specified_work",
            Self::Investigation => "investigation",
            Self::OpenDesign => "open_design",
        }
    }

    const fn criterion(self) -> &'static str {
        match self {
            Self::DirectAnswer => {
                "An explanation, opinion or answer that needs no tools and nothing built: from knowledge or from what the conversation already holds"
            }
            Self::SingleAction => {
                "One or two tool calls: one lookup, one command or one small edit"
            }
            Self::SpecifiedWork => {
                "Several steps whose content the request already determines: the same edit across files, a precise spec to implement, checks to run and fix"
            }
            Self::Investigation => {
                "Several steps to find something out first: search across files, find the cause of a failure, then fix what was found"
            }
            Self::OpenDesign => {
                "Something to build where decisions without one right answer must be settled first: architecture, approach, unclear requirements"
            }
        }
    }

    const fn multi_step(self) -> bool {
        matches!(
            self,
            Self::SpecifiedWork | Self::Investigation | Self::OpenDesign
        )
    }
}

/// The work-shape battery: the shape and whether parts are independent.
pub fn work_shape_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let criteria: BTreeMap<String, Json> = WorkShape::ALL
        .iter()
        .map(|shape| (shape.id().to_owned(), Json::from(shape.criterion())))
        .collect();
    let mut questions = BTreeMap::new();
    questions.insert(
        WORK_SHAPE_QUESTION.to_owned(),
        Question::choice(
            "What shape of work does this request need from the agent? Judge the whole request.",
            criteria,
        )?,
    );
    questions.insert(
        DELEGATION_PARALLEL_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "Does this task contain independent parts that could run in parallel?",
            "Parts are independent",
            "The work is sequential",
        ),
    );
    Ok(questions)
}

/// What the main model is told about delegating this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationAdvice {
    /// One or two calls, or a plain answer: a subagent costs more than it saves.
    None,
    /// Work the request already specifies: hand it to the worker.
    Delegate { parallel: bool },
    /// Something to find out first: hand the investigation over with a precise question.
    Investigate { parallel: bool },
    /// Open decisions: settle them, then hand the building over.
    SettleThenDelegate,
}

impl DelegationAdvice {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "single",
            Self::Delegate { parallel: false } => "delegate",
            Self::Delegate { parallel: true } => "delegate:parallel",
            Self::Investigate { parallel: false } => "investigate",
            Self::Investigate { parallel: true } => "investigate:parallel",
            Self::SettleThenDelegate => "settle_then_delegate",
        }
    }
}

/// The advice for a request, or `None` without a usable answer.
pub fn compose_work_shape(answers: &JevAnswerSet) -> Option<DelegationAdvice> {
    let Some(Answer::Choice { probabilities, .. }) = answers.answers.get(WORK_SHAPE_QUESTION)
    else {
        return None;
    };
    let parallel = match answers.answers.get(DELEGATION_PARALLEL_QUESTION) {
        Some(Answer::Noul { noul }) => *noul >= PARALLEL_FLOOR,
        _ => return None,
    };
    let mass: Vec<(WorkShape, f64)> = WorkShape::ALL
        .iter()
        .map(|shape| {
            let p = probabilities.get(shape.id()).copied().unwrap_or(0.0);
            (*shape, if p.is_finite() { p.max(0.0) } else { 0.0 })
        })
        .collect();
    let total: f64 = mass.iter().map(|(_, p)| p).sum();
    if total <= 0.0 {
        return None;
    }
    let multi: f64 = mass
        .iter()
        .filter(|(shape, _)| shape.multi_step())
        .map(|(_, p)| p)
        .sum();
    if multi / total < MULTI_STEP_FLOOR {
        return Some(DelegationAdvice::None);
    }
    let (top, _) = mass.iter().filter(|(shape, _)| shape.multi_step()).fold(
        None::<(WorkShape, f64)>,
        |best, (shape, p)| match best {
            Some((_, q)) if q >= *p => best,
            _ => Some((*shape, *p)),
        },
    )?;
    Some(match top {
        WorkShape::OpenDesign => DelegationAdvice::SettleThenDelegate,
        WorkShape::Investigation => DelegationAdvice::Investigate { parallel },
        _ => DelegationAdvice::Delegate { parallel },
    })
}

// ---------------------------------------------------------------------------
// B7 — what kind of assignment a read-only child gets
// ---------------------------------------------------------------------------

/// Question id of the child task kind.
pub const CHILD_TASK_QUESTION: &str = "task_kind";
/// Share of reading kinds (lookup, summary) needed to run the child on the utility.
pub const UTILITY_CHILD_FLOOR: f64 = 0.75;

const CHILD_KINDS: [(&str, &str, bool); 3] = [
    (
        "lookup",
        "Find, list, read or quote specific code, files, configuration values or documentation; the report is what was found",
        true,
    ),
    (
        "summary",
        "Read a bounded amount of material and describe what it does, without judging whether it is right",
        true,
    ),
    (
        "analysis",
        "Explain a cause, trace behaviour across modules, compare options, or judge correctness or risk",
        false,
    ),
];

/// One `choice` about a read-only child's assignment.
pub fn child_task_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let criteria: BTreeMap<String, Json> = CHILD_KINDS
        .iter()
        .map(|(id, text, _)| ((*id).to_owned(), Json::from(*text)))
        .collect();
    Ok(BTreeMap::from([(
        CHILD_TASK_QUESTION.to_owned(),
        Question::choice(
            "What kind of assignment is `task`? It goes to a read-only subagent that searches and reads files and \
             reports back.",
            criteria,
        )?,
    )]))
}

/// `Some(true)` when the assignment is reading (lookup or summary) with at
/// least [`UTILITY_CHILD_FLOOR`] of the mass, `Some(false)` otherwise, `None`
/// without a usable answer.
pub fn compose_child_on_utility(answers: &JevAnswerSet) -> Option<bool> {
    let Some(Answer::Choice { probabilities, .. }) = answers.answers.get(CHILD_TASK_QUESTION)
    else {
        return None;
    };
    let mass = |reading: bool| -> f64 {
        CHILD_KINDS
            .iter()
            .filter(|(_, _, is_reading)| *is_reading == reading)
            .map(|(id, _, _)| probabilities.get(*id).copied().unwrap_or(0.0).max(0.0))
            .sum()
    };
    let (reading, other) = (mass(true), mass(false));
    let total = reading + other;
    (total > 0.0).then(|| reading / total >= UTILITY_CHILD_FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, noul};

    fn kinds(entries: &[(&str, f64)]) -> Answer {
        let probabilities: BTreeMap<String, f64> =
            entries.iter().map(|(k, p)| ((*k).to_owned(), *p)).collect();
        let choice = entries
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| (*k).to_owned())
            .unwrap_or_default();
        Answer::Choice {
            choice,
            probabilities,
            confidence: None,
        }
    }

    fn battery(kind: &[(&str, f64)], costly: f64) -> JevAnswerSet {
        answers(vec![
            (STEP_KIND_QUESTION, kinds(kind)),
            (WRONG_STEP_COSTLY_QUESTION, noul(costly)),
        ])
    }

    /// Claude's menu: low to max.
    fn claude_menu() -> Vec<OfferedLevel> {
        [
            ("low", rank::LOW),
            ("medium", rank::MEDIUM),
            ("high", rank::HIGH),
            ("xhigh", rank::XHIGH),
            ("max", rank::MAX),
        ]
        .into_iter()
        .map(|(id, rank)| OfferedLevel {
            id: id.to_owned(),
            rank,
        })
        .collect()
    }

    fn effort(kind: &[(&str, f64)], costly: f64, mode: ExecutionMode) -> String {
        compose_step_effort(&battery(kind, costly), &claude_menu(), mode, 0.5)
            .expect("a pick")
            .id
    }

    /// Reading and relaying never pay for deep thinking, in any mode: that is
    /// where most rounds of a session sit, so this is where the saving is.
    #[test]
    fn mechanical_steps_run_cheap_in_every_mode() {
        for mode in [
            ExecutionMode::Ultrasave,
            ExecutionMode::Normal,
            ExecutionMode::Ultrafast,
            ExecutionMode::Ultracode,
        ] {
            assert_eq!(
                effort(&[("lookup", 0.9), ("relay", 0.1)], 0.1, mode),
                "low",
                "{mode:?}"
            );
            assert_eq!(
                effort(&[("relay", 1.0)], 0.9, mode),
                "low",
                "{mode:?}: costly does not lift relaying"
            );
        }
    }

    /// The mode is a table row, not prose the decision model must weigh:
    /// the same diagnosis step runs cheaper in UltraSave and deeper in Normal.
    #[test]
    fn the_mode_row_sets_the_level_of_hard_steps() {
        let diagnose = [("diagnose", 0.9), ("lookup", 0.1)];
        assert_eq!(effort(&diagnose, 0.1, ExecutionMode::Ultrasave), "medium");
        assert_eq!(effort(&diagnose, 0.1, ExecutionMode::Normal), "high");
        assert_eq!(
            effort(&[("design", 1.0)], 0.1, ExecutionMode::Ultracode),
            "xhigh"
        );
        assert_eq!(
            effort(&[("implement", 1.0)], 0.1, ExecutionMode::Normal),
            "medium"
        );
    }

    /// An expensive mistake buys one level, never past high outside UltraCode:
    /// xhigh and max stay for a redo or the escalation ratchet.
    #[test]
    fn a_costly_step_climbs_one_level_up_to_the_ceiling() {
        assert_eq!(
            effort(&[("implement", 1.0)], 0.9, ExecutionMode::Normal),
            "high"
        );
        assert_eq!(
            effort(&[("design", 1.0)], 0.9, ExecutionMode::Normal),
            "high",
            "already at the ceiling"
        );
        assert_eq!(
            effort(&[("implement", 1.0)], 0.9, ExecutionMode::Ultrasave),
            "medium"
        );
        assert_eq!(
            effort(&[("diagnose", 1.0)], 0.9, ExecutionMode::Ultracode),
            "xhigh"
        );
        assert_eq!(
            effort(&[("implement", 1.0)], 0.5, ExecutionMode::Normal),
            "medium",
            "below the floor"
        );
    }

    /// A split answer lands between its readings, and the mode's quantile
    /// leans it: UltraSave reads lower, UltraCode higher.
    #[test]
    fn a_split_answer_follows_the_quantile() {
        let split = battery(&[("lookup", 0.5), ("diagnose", 0.5)], 0.1);
        let pick = |q| {
            compose_step_effort(&split, &claude_menu(), ExecutionMode::Normal, q)
                .unwrap()
                .id
        };
        assert_eq!(pick(0.4), "low");
        assert_eq!(pick(0.75), "high");
    }

    /// The target is mapped onto the model's own menu: a model without `low`
    /// runs a reading step at its cheapest level, not at an invented one.
    #[test]
    fn the_target_maps_onto_the_offered_menu() {
        let menu = vec![
            OfferedLevel {
                id: "minimal".into(),
                rank: rank::MINIMAL,
            },
            OfferedLevel {
                id: "medium".into(),
                rank: rank::MEDIUM,
            },
            OfferedLevel {
                id: "high".into(),
                rank: rank::HIGH,
            },
        ];
        let pick = |kind: &[(&str, f64)]| {
            compose_step_effort(&battery(kind, 0.1), &menu, ExecutionMode::Normal, 0.5)
                .unwrap()
                .id
        };
        assert_eq!(pick(&[("lookup", 1.0)]), "minimal");
        assert_eq!(
            pick(&[("relay", 1.0)]),
            "minimal",
            "low is nearest minimal on a tie"
        );
        assert_eq!(pick(&[("review", 1.0)]), "high");
    }

    #[test]
    fn no_step_kind_means_no_pick() {
        let none = answers(vec![(WRONG_STEP_COSTLY_QUESTION, noul(0.9))]);
        assert!(compose_step_effort(&none, &claude_menu(), ExecutionMode::Normal, 0.5).is_none());
        let unknown = answers(vec![(STEP_KIND_QUESTION, kinds(&[("other", 1.0)]))]);
        assert!(
            compose_step_effort(&unknown, &claude_menu(), ExecutionMode::Normal, 0.5).is_none()
        );
    }

    /// One battery answer also routes the round: mechanical steps to the
    /// cheapest model, code to the worker, judgment to the main model.
    #[test]
    fn the_same_answer_picks_the_tier() {
        let all = [tier::UTILITY, tier::WORKER, tier::MAIN];
        let tier_of = |kind: &[(&str, f64)], costly, offered: &[u8]| {
            compose_step_tier(&battery(kind, costly), offered, 0.5)
                .unwrap()
                .0
        };
        assert_eq!(tier_of(&[("lookup", 1.0)], 0.1, &all), tier::UTILITY);
        assert_eq!(tier_of(&[("implement", 1.0)], 0.1, &all), tier::WORKER);
        assert_eq!(tier_of(&[("diagnose", 1.0)], 0.1, &all), tier::MAIN);
        assert_eq!(
            tier_of(&[("implement", 1.0)], 0.9, &all),
            tier::MAIN,
            "a costly edit stays on main"
        );
        assert_eq!(
            tier_of(&[("lookup", 1.0)], 0.1, &[tier::WORKER, tier::MAIN]),
            tier::WORKER
        );
        assert_eq!(
            tier_of(&[("implement", 1.0)], 0.1, &[tier::UTILITY, tier::MAIN]),
            tier::MAIN
        );
    }

    fn shape(entries: &[(&str, f64)], parallel: f64) -> JevAnswerSet {
        answers(vec![
            (WORK_SHAPE_QUESTION, kinds(entries)),
            (DELEGATION_PARALLEL_QUESTION, noul(parallel)),
        ])
    }

    /// A plain answer or one command is cheaper done directly than delegated.
    #[test]
    fn short_work_gets_no_delegation_hint() {
        assert_eq!(
            compose_work_shape(&shape(&[("direct_answer", 0.7), ("open_design", 0.3)], 0.1)),
            Some(DelegationAdvice::None)
        );
        assert_eq!(
            compose_work_shape(&shape(
                &[("single_action", 0.9), ("specified_work", 0.1)],
                0.9
            )),
            Some(DelegationAdvice::None)
        );
    }

    /// The hint says what to hand over: built work, an investigation, or
    /// the building once the open decisions are settled.
    #[test]
    fn the_shape_picks_what_to_hand_over() {
        assert_eq!(
            compose_work_shape(&shape(
                &[("specified_work", 0.8), ("single_action", 0.2)],
                0.2
            )),
            Some(DelegationAdvice::Delegate { parallel: false })
        );
        assert_eq!(
            compose_work_shape(&shape(
                &[("specified_work", 0.8), ("single_action", 0.2)],
                0.8
            )),
            Some(DelegationAdvice::Delegate { parallel: true })
        );
        assert_eq!(
            compose_work_shape(&shape(
                &[("investigation", 0.7), ("specified_work", 0.2)],
                0.1
            )),
            Some(DelegationAdvice::Investigate { parallel: false })
        );
        assert_eq!(
            compose_work_shape(&shape(
                &[("open_design", 0.6), ("specified_work", 0.3)],
                0.9
            )),
            Some(DelegationAdvice::SettleThenDelegate)
        );
        assert!(
            compose_work_shape(&answers(vec![(
                WORK_SHAPE_QUESTION,
                kinds(&[("specified_work", 1.0)])
            )]))
            .is_none(),
            "needs both answers"
        );
    }

    /// Only plain reading goes to the utility model: anything that judges
    /// stays on the worker, where a wrong conclusion is less likely.
    #[test]
    fn only_reading_assignments_run_on_the_utility() {
        let child = |entries: &[(&str, f64)]| {
            compose_child_on_utility(&answers(vec![(CHILD_TASK_QUESTION, kinds(entries))]))
        };
        assert_eq!(child(&[("lookup", 0.9), ("analysis", 0.1)]), Some(true));
        assert_eq!(
            child(&[("lookup", 0.4), ("summary", 0.4), ("analysis", 0.2)]),
            Some(true)
        );
        assert_eq!(child(&[("summary", 0.6), ("analysis", 0.4)]), Some(false));
        assert_eq!(child(&[]), None);
    }

    #[test]
    fn the_questions_build() {
        assert_eq!(step_battery_questions().unwrap().len(), 2);
        assert_eq!(work_shape_questions().unwrap().len(), 2);
        assert_eq!(child_task_questions().unwrap().len(), 1);
    }
}
