// Modified for Distill by Samuel Fajreldines, 2026.
//! Escalation ratchet: harness signals that a lean turn needs more capacity.
//!
//! Every trigger is recorded in shadow, as a content-free `escalation`
//! decision row labelled `shadow:<trigger>`, at most once per turn and only in
//! root sessions, so its fire rate can be measured before it changes anything.
//! Only the triggers named in `[modes] escalation` act. They act without Jev
//! (every signal is a harness fact or an answer Jev already gave), move up
//! only, and climb at most twice per turn. The state lives in the turn ledger and is dropped when the turn ends.
//!
//! Ladders, by mode: UltraSave raises the executor's effort one level, then
//! runs one read-only review on the session's model over the turn's patch.
//! UltraFast raises the effort one level, then runs the rest of the turn under
//! Normal's effort policy. Under Auto each climb moves the effective mode one
//! step up, and the mode does not come back down before the next request.

use distill_sampling_types::ReasoningEffort;
use distill_tool_types::ExecutionMode;
use distill_tools::types::output::ToolOutput;
use distill_workspace::jev::catalog::verify::DiffReviewVerdict;
use distill_workspace::jev::flags::JevLever;

use super::turn_facts::CheckOutcome;
use super::*;
use crate::session::goal_classifier::turn_snapshot::TurnSnapshot;

/// Tool rounds in a row with no new working-tree change and no passing check
/// before `stalled_rounds` fires.
pub(crate) const STALLED_ROUNDS: u32 = 8;
/// Climbs a turn may make.
const MAX_CLIMBS: u8 = 2;

/// A harness signal that the turn may need more capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationTrigger {
    /// A request on a tier-routed round failed.
    RoutedError,
    /// C4 found a break, a mismatch or an unfinished change, above its own
    /// floor, in an edit that is not prose.
    C4Break,
    /// The same recorded check failed again after the working tree changed.
    CheckFailedTwice,
    /// The harness nudged the model out of a run of identical tool calls.
    IdenticalCalls,
    /// [`STALLED_ROUNDS`] tool rounds with no new change and no passing check.
    StalledRounds,
}

impl EscalationTrigger {
    const ALL: [Self; 5] = [
        Self::RoutedError,
        Self::C4Break,
        Self::CheckFailedTwice,
        Self::IdenticalCalls,
        Self::StalledRounds,
    ];

    /// The name `[modes] escalation` uses.
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::RoutedError => "routed_error",
            Self::C4Break => "c4_break",
            Self::CheckFailedTwice => "check_failed_twice",
            Self::IdenticalCalls => "identical_calls",
            Self::StalledRounds => "stalled_rounds",
        }
    }

    /// The label of the trigger's shadow row.
    pub(crate) const fn shadow_label(self) -> &'static str {
        match self {
            Self::RoutedError => "shadow:routed_error",
            Self::C4Break => "shadow:c4_break",
            Self::CheckFailedTwice => "shadow:check_failed_twice",
            Self::IdenticalCalls => "shadow:identical_calls",
            Self::StalledRounds => "shadow:stalled_rounds",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|trigger| trigger.id() == id)
    }
}

/// The triggers that act; every other one is only recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LiveTriggers(u8);

impl LiveTriggers {
    pub(crate) fn of(triggers: &[EscalationTrigger]) -> Self {
        Self(
            triggers
                .iter()
                .fold(0, |bits, trigger| bits | trigger.bit()),
        )
    }

    pub(crate) fn contains(self, trigger: EscalationTrigger) -> bool {
        self.0 & trigger.bit() != 0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The triggers `root`, an effective config, names in `[modes] escalation`,
/// and the entries it names that are not triggers.
pub(crate) fn live_triggers_in(root: Option<&toml::Value>) -> (LiveTriggers, Vec<String>) {
    let mut live = LiveTriggers::default();
    let mut unknown = Vec::new();
    let Some(value) = root.and_then(|root| root.get("modes")?.get("escalation")) else {
        return (live, unknown);
    };
    let Some(names) = value.as_array() else {
        unknown.push(value.to_string());
        return (live, unknown);
    };
    for name in names {
        match name
            .as_str()
            .map(str::trim)
            .and_then(EscalationTrigger::from_id)
        {
            Some(trigger) => live.0 |= trigger.bit(),
            None => unknown.push(name.to_string()),
        }
    }
    (live, unknown)
}

#[cfg(test)]
thread_local! {
    static LIVE_FOR_TESTS: std::cell::Cell<Option<LiveTriggers>> = const { std::cell::Cell::new(None) };
}

/// Overrides the configured live triggers on this thread; `None` reads the
/// config.
#[cfg(test)]
pub(crate) fn set_live_triggers_for_tests(triggers: Option<&[EscalationTrigger]>) {
    LIVE_FOR_TESTS.with(|cell| cell.set(triggers.map(LiveTriggers::of)));
}

/// The live triggers in the effective config, read once per process (config
/// does not change mid-run). Unknown names are ignored with one warning; a
/// config that fails to load leaves every trigger in shadow.
pub(crate) fn live_triggers() -> LiveTriggers {
    #[cfg(test)]
    if let Some(live) = LIVE_FOR_TESTS.with(std::cell::Cell::get) {
        return live;
    }
    static SETTING: std::sync::OnceLock<LiveTriggers> = std::sync::OnceLock::new();
    *SETTING.get_or_init(|| {
        let (live, unknown) =
            live_triggers_in(crate::config::load_effective_config().ok().as_ref());
        if !unknown.is_empty() {
            tracing::warn!(
                ?unknown,
                "[modes] escalation: ignoring entries that are not triggers"
            );
        }
        live
    })
}

const fn mode_id(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Normal => "normal",
        ExecutionMode::Ultrafast => "ultrafast",
        ExecutionMode::Ultracode => "ultracode",
        ExecutionMode::Ultrasave => "ultrasave",
    }
}

/// Lean modes, then Normal, then UltraCode.
const fn mode_rank(mode: ExecutionMode) -> u8 {
    match mode {
        ExecutionMode::Ultrasave | ExecutionMode::Ultrafast => 0,
        ExecutionMode::Normal => 1,
        ExecutionMode::Ultracode => 2,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ReviewStep {
    #[default]
    None,
    Requested,
    Done,
}

/// What `stalled_rounds` needs to know about the rounds so far. The tree
/// marker after a round is taken in the background while the next request
/// runs, so a round is judged one round late and never waits on git.
#[derive(Debug, Default)]
struct StallTracker {
    last_marker: Option<String>,
    /// The marker being taken after the last round, and whether a check
    /// passed during that round.
    pending: Option<(tokio::task::JoinHandle<Option<String>>, bool)>,
    quiet_rounds: u32,
    last_round_end: Option<std::time::SystemTime>,
}

/// The ratchet's state for one turn.
#[derive(Debug, Default)]
pub(crate) struct EscalationState {
    /// Triggers already recorded this turn.
    recorded: u8,
    climbs: u8,
    /// The effort the rest of the turn runs at least at, as `(level, value)`.
    effort_level: Option<(String, ReasoningEffort)>,
    /// UltraFast's second climb: Normal's effort policy for the rest of the turn.
    normal_effort_policy: bool,
    /// Under Auto, the mode a climb reached; the request does not go below it.
    auto_floor: Option<ExecutionMode>,
    review: ReviewStep,
    /// The tree before the turn's first round, for UltraSave's review.
    snapshot: Option<TurnSnapshot>,
    stall: StallTracker,
}

impl EscalationState {
    /// Climbs this turn, for the turn outcome record.
    pub(crate) fn escalations(&self) -> u32 {
        u32::from(self.climbs)
    }
}

fn record(label: &str, reason: &str) {
    crate::jev::record_item(JevLever::Escalation, label, reason, None, None);
}

/// Paths an executed edit tool wrote.
fn edited_paths(output: &ToolOutput) -> Vec<&std::path::Path> {
    use distill_tools::types::output::{ApplyPatchOutput, SearchReplaceOutput};
    match output {
        ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(edit)) => {
            vec![edit.absolute_path.as_path()]
        }
        ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => files
            .iter()
            .flat_map(|file| std::iter::once(file.path.as_path()).chain(file.move_to.as_deref()))
            .collect(),
        _ => Vec::new(),
    }
}

impl SessionActor {
    /// Records `trigger` once per turn in a root session, and climbs when it
    /// is live.
    pub(super) fn note_escalation(&self, trigger: EscalationTrigger) {
        if self.startup_hints.is_subagent {
            return;
        }
        {
            let mut ledger = self.jev_ledger.borrow_mut();
            let state = &mut ledger.escalation;
            if state.recorded & trigger.bit() != 0 {
                return;
            }
            state.recorded |= trigger.bit();
        }
        record(trigger.shadow_label(), trigger.id());
        if live_triggers().contains(trigger) {
            self.escalation_climb(trigger);
        }
    }

    /// `c4_break`: C4's verdict on an executed edit that is not only prose.
    pub(super) fn note_c4_escalation(
        &self,
        verdict: DiffReviewVerdict,
        confidence: Option<f64>,
        output: &ToolOutput,
    ) {
        if verdict == DiffReviewVerdict::Ok || confidence.is_none() {
            return;
        }
        let root = self.tool_context.cwd.as_path();
        let paths = edited_paths(output);
        let prose = |path: &&std::path::Path| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            crate::session::goal_classifier::turn_review::is_prose_path(
                &relative.to_string_lossy().replace('\\', "/"),
            )
        };
        if paths.is_empty() || paths.iter().all(prose) {
            return;
        }
        self.note_escalation(EscalationTrigger::C4Break);
    }

    /// One step of the mode's ladder for a live trigger.
    fn escalation_climb(&self, trigger: EscalationTrigger) {
        let climbs = self.jev_ledger.borrow().escalation.climbs;
        if climbs >= MAX_CLIMBS {
            return;
        }
        if self.execution_mode.is_auto() {
            if let Some(mode) = self.escalate_auto_mode() {
                {
                    let mut ledger = self.jev_ledger.borrow_mut();
                    ledger.escalation.climbs += 1;
                    ledger.escalation.auto_floor = Some(mode);
                }
                record(&format!("climb:mode:{}", mode_id(mode)), trigger.id());
            }
            return;
        }
        match (self.execution_mode.load(), climbs) {
            (ExecutionMode::Ultrasave | ExecutionMode::Ultrafast, 0) => {
                let level = self.escalation_next_effort();
                let reason = level
                    .as_ref()
                    .map_or("no higher effort level", |(level, _)| level.as_str())
                    .to_owned();
                {
                    let mut ledger = self.jev_ledger.borrow_mut();
                    ledger.escalation.climbs = 1;
                    if level.is_some() {
                        ledger.escalation.effort_level = level;
                    }
                }
                record("climb:effort", &reason);
            }
            (ExecutionMode::Ultrasave, _) => {
                {
                    let mut ledger = self.jev_ledger.borrow_mut();
                    ledger.escalation.climbs = MAX_CLIMBS;
                    ledger.escalation.review = ReviewStep::Requested;
                }
                record("climb:review", trigger.id());
            }
            (ExecutionMode::Ultrafast, _) => {
                {
                    let mut ledger = self.jev_ledger.borrow_mut();
                    ledger.escalation.climbs = MAX_CLIMBS;
                    ledger.escalation.normal_effort_policy = true;
                }
                record("climb:normal_effort", trigger.id());
            }
            // Normal and UltraCode give up no capacity: nothing to climb.
            _ => {}
        }
    }

    /// One effort level above what the last round ran at, on its model.
    fn escalation_next_effort(&self) -> Option<(String, ReasoningEffort)> {
        let (model, effort) = self.jev_ledger.borrow().last_execution.clone()?;
        let effort = effort.or_else(|| self.models_manager.current_reasoning_effort());
        let level = self.next_effort_level_above(&model, effort)?;
        let value = self
            .models_manager
            .model_reasoning_efforts(&model)
            .into_iter()
            .find(|entry| entry.id == level)?
            .value;
        Some((level, value))
    }

    /// Raises this round's effort to the ratchet's level. An explicit effort
    /// keeps its authority, a model without that level keeps its own, and a
    /// higher effort already chosen stays.
    pub(super) fn escalation_apply_effort_floor(&self, cfg: &mut SamplingConfig) {
        if self.startup_hints.is_subagent
            || !self
                .jev_effort_auto
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let Some((_, value)) = self.jev_ledger.borrow().escalation.effort_level.clone() else {
            return;
        };
        if !self
            .models_manager
            .model_supports_reasoning_effort_value(&cfg.model, value)
        {
            return;
        }
        let current = cfg
            .reasoning_effort
            .or_else(|| self.models_manager.current_reasoning_effort());
        if current.is_some_and(|current| effort_rank(current) >= effort_rank(value)) {
            return;
        }
        cfg.reasoning_effort = Some(value);
        if !self.child_model_routing_locked()
            && let Some(model_id) = self.models_manager.model_for_effort(&cfg.model, value)
        {
            cfg.model = model_id;
        }
    }

    /// The mode whose effort policy this round follows.
    pub(super) fn escalation_effort_mode(&self) -> ExecutionMode {
        if self.jev_ledger.borrow().escalation.normal_effort_policy {
            ExecutionMode::Normal
        } else {
            self.execution_mode.load()
        }
    }

    /// Whether Auto's review must keep the mode instead of moving to `mode`:
    /// a climb this request reached a higher one.
    pub(super) fn escalation_holds_auto_mode(&self, mode: ExecutionMode) -> bool {
        self.jev_ledger
            .borrow()
            .escalation
            .auto_floor
            .is_some_and(|floor| mode_rank(mode) < mode_rank(floor))
    }

    /// Before a round's routing: a pinned UltraSave turn with live triggers
    /// snapshots the tree once, so its review sees only this turn's work.
    pub(super) async fn escalation_before_round(&self) {
        if self.startup_hints.is_subagent
            || self.execution_mode.is_auto()
            || self.execution_mode.load() != ExecutionMode::Ultrasave
            || live_triggers().is_empty()
            || self.jev_ledger.borrow().escalation.snapshot.is_some()
        {
            return;
        }
        let snapshot = crate::session::goal_classifier::turn_snapshot::capture_turn_snapshot(
            self.tool_context.cwd.as_path(),
            crate::session::goal_classifier::turn_review::SNAPSHOT_BUDGET,
        )
        .await;
        self.jev_ledger.borrow_mut().escalation.snapshot = Some(snapshot);
    }

    /// After a tool round: the triggers read from the turn's facts, then the
    /// review a climb asked for.
    pub(super) async fn escalation_after_round(&self) {
        if self.startup_hints.is_subagent {
            return;
        }
        let failed_again = self
            .jev_ledger
            .borrow()
            .facts
            .recorded_checks()
            .iter()
            .any(|check| {
                check.outcome == CheckOutcome::Failed
                    && check.failed_again_after_change == Some(true)
            });
        if failed_again {
            self.note_escalation(EscalationTrigger::CheckFailedTwice);
        }
        if self.escalation_stalled().await {
            self.note_escalation(EscalationTrigger::StalledRounds);
        }
        Box::pin(self.escalation_review()).await;
    }

    /// Judges the round before this one and starts the tree marker for this
    /// one; `true` once [`STALLED_ROUNDS`] judged rounds in a row changed
    /// nothing and passed no check. A missing marker (no repository, git
    /// error) is no evidence of a stall.
    async fn escalation_stalled(&self) -> bool {
        let (pending, since) = {
            let mut ledger = self.jev_ledger.borrow_mut();
            let stall = &mut ledger.escalation.stall;
            (stall.pending.take(), stall.last_round_end)
        };
        let passed_now = self
            .jev_ledger
            .borrow()
            .facts
            .recorded_checks()
            .iter()
            .any(|check| {
                check.outcome == CheckOutcome::Passed
                    && since.is_none_or(|since| check.finished_at > since)
            });
        let mut stalled = false;
        if let Some((handle, passed)) = pending {
            let marker = handle.await.ok().flatten();
            let mut ledger = self.jev_ledger.borrow_mut();
            let stall = &mut ledger.escalation.stall;
            let quiet = !passed && marker.is_some() && marker == stall.last_marker;
            stall.quiet_rounds = if quiet { stall.quiet_rounds + 1 } else { 0 };
            stall.last_marker = marker;
            stalled = stall.quiet_rounds >= STALLED_ROUNDS;
        }
        let root = self.tool_context.cwd.as_path().to_path_buf();
        let handle = tokio::spawn(async move { super::turn_facts::worktree_marker(&root).await });
        let mut ledger = self.jev_ledger.borrow_mut();
        let stall = &mut ledger.escalation.stall;
        stall.pending = Some((handle, passed_now));
        stall.last_round_end = Some(std::time::SystemTime::now());
        stalled
    }

    /// UltraSave's second climb: one read-only review of the turn's patch on
    /// the session's model; its gaps join the conversation once.
    async fn escalation_review(&self) {
        use crate::session::goal_classifier::turn_review::{TurnReviewVerdict, has_non_prose};
        let snapshot = {
            let mut ledger = self.jev_ledger.borrow_mut();
            let state = &mut ledger.escalation;
            if state.review != ReviewStep::Requested {
                return;
            }
            state.review = ReviewStep::Done;
            state.snapshot.clone()
        };
        let root = self.tool_context.cwd.as_path().to_path_buf();
        let patch = match snapshot {
            None | Some(TurnSnapshot::NoRepository) => Err("no turn snapshot".to_owned()),
            Some(snapshot) => {
                crate::session::goal_classifier::turn_snapshot::capture_turn_patch(&root, &snapshot)
                    .await
            }
        };
        let patch = match patch {
            Ok(patch) if has_non_prose(&patch.changes.changed_files) => patch,
            Ok(_) => return record("review:skip-prose", "the turn changed only prose"),
            Err(error) => return record("review:unavailable", &error),
        };
        let verifier_id = format!("escalation-{}", uuid::Uuid::new_v4().simple());
        let verdict = Box::pin(self.run_turn_patch_review(
            &root,
            &patch,
            super::goal::TurnPatchReviewRun {
                verifier_id: &verifier_id,
                review: 1,
                prior_gaps: None,
                skeptic_count: 1,
                escalation: true,
            },
        ))
        .await;
        let _ = tokio::fs::remove_dir_all(crate::session::goal_tracker::goal_scratch_root(
            &verifier_id,
        ))
        .await;
        match verdict {
            TurnReviewVerdict::NotRefuted => record("review:not_refuted", ""),
            TurnReviewVerdict::Unavailable { reason } => record("review:unavailable", &reason),
            TurnReviewVerdict::Refuted { gaps, .. } | TurnReviewVerdict::Blocked { gaps } => {
                record("review:refuted", "");
                self.push_system_reminder(&format!(
                    "An independent read-only review of this turn's changes found gaps. \
                     Fix them and re-run the checks that cover the changed code before you \
                     give your final answer:\n{}",
                    crate::session::goal_classifier::neutralize_reminder_tags(gaps)
                ));
            }
        }
    }
}

#[cfg(test)]
#[path = "escalation_tests.rs"]
mod tests;
