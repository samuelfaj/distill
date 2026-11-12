//! Floors the harness compares Jev's probabilities with, and their provenance.
//!
//! The served Jev model changes version behind its alias, and a floor fitted
//! on one version says nothing about the next, so every floor here is read
//! through [`thresholds_for`] with the model id that answered. A model id
//! without an entry in [`CALIBRATIONS`] gets [`LeverThresholds::DEFAULT`] (the
//! numbers the harness shipped with) and is reported as `uncalibrated`.
//!
//! A calibrated value names the question it was fitted on by its instruction
//! hash; a test fails when that question's wording no longer hashes the same,
//! because a floor fitted on other wording is not a fit for this one. A value
//! enters the table only after the replay, shadow and paired-benchmark
//! sequence in `docs/jev-routing.md`.

use std::collections::BTreeMap;

use crate::jev::types::{Question, QuestionId};

/// The floors of the mode review, the turn-end final check (C1, C3) and the
/// diff review (C4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeverThresholds {
    /// B8 mid-request review: probability on a new mode needed to switch to it.
    pub mode_review_switch: f64,
    /// B8 mid-request review under `b8_up_only`: how far the probability on a
    /// higher mode must exceed the current mode's for Auto to climb to it.
    pub mode_review_up_margin: f64,
    /// C1: probability above which a requested item counts as done; the final
    /// check also needs this much confidence that work remains.
    pub c1_item_done: f64,
    /// C3: per-item probability above which a request item counts as satisfied.
    pub c3_item_satisfied: f64,
    /// C3: "something requested is still missing" must be below this to call it done.
    pub c3_leftover: f64,
    /// C4: a mismatch is reported only when P(change matches the step) is below this.
    pub c4_match: f64,
    /// C4: probability at or above which a red flag (break, unfinished) is reported.
    pub c4_flag: f64,
    /// C4: the step is redone only when P(step done) is below this.
    pub c4_step_complete: f64,
    /// C4: probability at or above which the redo needs more thinking.
    pub c4_redo_higher: f64,
    /// C4: probability at or above which the review asks for another model.
    pub c4_second_opinion: f64,
}

impl LeverThresholds {
    /// The floors every Jev model id gets until it is calibrated.
    pub const DEFAULT: Self = Self {
        mode_review_switch: 0.6,
        // Not fitted: the value the up-only rule was designed with.
        mode_review_up_margin: 0.25,
        c1_item_done: 0.60,
        c3_item_satisfied: 0.70,
        c3_leftover: 0.30,
        c4_match: 0.30,
        c4_flag: 0.70,
        c4_step_complete: 0.30,
        c4_redo_higher: 0.70,
        c4_second_opinion: 0.70,
    };

    fn set(&mut self, threshold: Threshold, value: f64) {
        let field = match threshold {
            Threshold::ModeReviewSwitch => &mut self.mode_review_switch,
            Threshold::ModeReviewUpMargin => &mut self.mode_review_up_margin,
            Threshold::C1ItemDone => &mut self.c1_item_done,
            Threshold::C3ItemSatisfied => &mut self.c3_item_satisfied,
            Threshold::C3Leftover => &mut self.c3_leftover,
            Threshold::C4Match => &mut self.c4_match,
            Threshold::C4Flag => &mut self.c4_flag,
            Threshold::C4StepComplete => &mut self.c4_step_complete,
            Threshold::C4RedoHigher => &mut self.c4_redo_higher,
            Threshold::C4SecondOpinion => &mut self.c4_second_opinion,
        };
        *field = value;
    }
}

impl Default for LeverThresholds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One field of [`LeverThresholds`], by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    ModeReviewSwitch,
    ModeReviewUpMargin,
    C1ItemDone,
    C3ItemSatisfied,
    C3Leftover,
    C4Match,
    C4Flag,
    C4StepComplete,
    C4RedoHigher,
    C4SecondOpinion,
}

/// The question pack whose answers a threshold is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdPack {
    /// The B8 mode question; it is built in the shell crate.
    ModeQuestion,
    /// C1 as the final check asks it (one item, `user request`).
    PrematureStop,
    /// C3 as the final check asks it (one item, `user request`).
    Completion,
    /// The C4 diff review battery.
    DiffReview,
}

impl Threshold {
    pub const fn pack(self) -> ThresholdPack {
        match self {
            Self::ModeReviewSwitch | Self::ModeReviewUpMargin => ThresholdPack::ModeQuestion,
            Self::C1ItemDone => ThresholdPack::PrematureStop,
            Self::C3ItemSatisfied | Self::C3Leftover => ThresholdPack::Completion,
            Self::C4Match
            | Self::C4Flag
            | Self::C4StepComplete
            | Self::C4RedoHigher
            | Self::C4SecondOpinion => ThresholdPack::DiffReview,
        }
    }
}

/// A floor fitted for one served Jev model id, with what it was fitted on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// The served model id exactly as an answer set reports it.
    pub model_id: &'static str,
    pub threshold: Threshold,
    pub value: f64,
    /// First and last day (`YYYY-MM-DD`) of the decisions the fit used.
    pub fit_from: &'static str,
    pub fit_to: &'static str,
    /// Labelled decisions in the fit.
    pub n: u32,
    /// [`question_hash`] of the pack the fit's decisions answered.
    pub question_hash: &'static str,
}

/// Fitted floors. Empty: every model id runs on the defaults.
pub const CALIBRATIONS: &[Calibration] = &[];

/// The floors for the Jev model that answered.
pub fn thresholds_for(model_id: &str) -> LeverThresholds {
    thresholds_in(CALIBRATIONS, model_id)
}

/// [`thresholds_for`] over an explicit table.
pub fn thresholds_in(table: &[Calibration], model_id: &str) -> LeverThresholds {
    let mut thresholds = LeverThresholds::DEFAULT;
    for entry in table.iter().filter(|entry| entry.model_id == model_id) {
        thresholds.set(entry.threshold, entry.value);
    }
    thresholds
}

/// `calibrated` when every one of `thresholds` has a fitted value for
/// `model_id`, else `uncalibrated`: the word decision logs carry next to the
/// floors a decision was taken on.
pub fn calibration_word(model_id: &str, thresholds: &[Threshold]) -> &'static str {
    calibration_word_in(CALIBRATIONS, model_id, thresholds)
}

/// [`calibration_word`] over an explicit table.
pub fn calibration_word_in(
    table: &[Calibration],
    model_id: &str,
    thresholds: &[Threshold],
) -> &'static str {
    let fitted = |threshold: &Threshold| {
        table
            .iter()
            .any(|entry| entry.model_id == model_id && entry.threshold == *threshold)
    };
    if !thresholds.is_empty() && thresholds.iter().all(fitted) {
        "calibrated"
    } else {
        "uncalibrated"
    }
}

/// 12 hex over the question ids and instruction texts, in id order: the same
/// value a `jev_decision` event carries as `question_hash` for this pack.
pub fn question_hash(questions: &BTreeMap<QuestionId, Question>) -> String {
    use sha2::{Digest, Sha256};
    let mut bytes = Vec::new();
    for (id, question) in questions {
        let (Question::Noul { instructions, .. }
        | Question::Choice { instructions, .. }
        | Question::Score { instructions, .. }) = question;
        bytes.extend_from_slice(id.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(instructions.to_string().as_bytes());
        bytes.push(0);
    }
    Sha256::digest(&bytes)
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The live hash of a pack this crate builds; `None` for the mode question,
/// which the shell crate builds and checks.
pub fn live_question_hash(pack: ThresholdPack) -> Option<String> {
    use super::verify;
    let items = ["user request".to_owned()];
    let questions = match pack {
        ThresholdPack::ModeQuestion => return None,
        ThresholdPack::PrematureStop => verify::premature_stop_questions(&items).ok()?,
        ThresholdPack::Completion => verify::completion_questions(&items).ok()?,
        ThresholdPack::DiffReview => verify::diff_review_request("step", "change").ok()?.1,
    };
    Some(question_hash(&questions))
}

/// Entries whose question no longer hashes as it did when they were fitted.
/// `live` gives the current hash of a pack; `None` counts as changed.
pub fn stale_calibrations<'a>(
    table: &'a [Calibration],
    live: impl Fn(ThresholdPack) -> Option<String>,
) -> Vec<&'a Calibration> {
    table
        .iter()
        .filter(|entry| live(entry.threshold.pack()).as_deref() != Some(entry.question_hash))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNKNOWN_MODEL: &str = "jev-not-in-the-table";

    /// With the table empty every floor is the number the harness shipped
    /// with, so no decision changes: one assertion per moved floor, against
    /// the literal that stood at its old site.
    #[test]
    fn an_uncalibrated_model_reads_the_numbers_the_harness_shipped_with() {
        let t = thresholds_for(UNKNOWN_MODEL);
        assert_eq!(
            t.mode_review_switch, 0.6,
            "jev_auto_mode.rs SWITCH_PROBABILITY"
        );
        assert_eq!(
            t.mode_review_up_margin, 0.25,
            "the up-only rule's design value"
        );
        assert_eq!(t.c1_item_done, 0.60, "verify.rs ITEM_DONE_FLOOR");
        assert_eq!(t.c3_item_satisfied, 0.70, "verify.rs ITEM_SATISFIED_FLOOR");
        assert_eq!(t.c3_leftover, 0.30, "verify.rs LEFTOVER_FLOOR");
        assert_eq!(t.c4_match, 0.30, "verify.rs DIFF_MATCH_FLOOR");
        assert_eq!(t.c4_flag, 0.70, "verify.rs DIFF_REVIEW_FLAG_FLOOR");
        assert_eq!(t.c4_step_complete, 0.30, "verify.rs STEP_COMPLETE_FLOOR");
        assert_eq!(t.c4_redo_higher, 0.70, "verify.rs REDO_HIGHER_FLOOR");
        assert_eq!(t.c4_second_opinion, 0.70, "verify.rs SECOND_OPINION_FLOOR");
        assert_eq!(t, thresholds_for(""), "an empty model id is unknown too");
        let stop = super::super::verify::compose_premature_stop(
            &super::super::test_support::answers(Vec::new()),
            &[],
        );
        assert_eq!(
            stop.done_floor, 0.60,
            "the final check's C1 gate reads the same floor"
        );
        assert_eq!(
            calibration_word(UNKNOWN_MODEL, &[Threshold::C4Flag]),
            "uncalibrated"
        );
    }

    /// A fitted value applies to its model id and threshold only.
    #[test]
    fn a_calibration_applies_to_its_model_and_threshold_only() {
        let table = [Calibration {
            model_id: "jev-9.9",
            threshold: Threshold::C4Flag,
            value: 0.55,
            fit_from: "2026-01-01",
            fit_to: "2026-01-14",
            n: 120,
            question_hash: "000000000000",
        }];
        let fitted = thresholds_in(&table, "jev-9.9");
        assert_eq!(fitted.c4_flag, 0.55);
        assert_eq!(
            fitted.c4_second_opinion,
            LeverThresholds::DEFAULT.c4_second_opinion
        );
        assert_eq!(thresholds_in(&table, "jev-9.8"), LeverThresholds::DEFAULT);
        assert_eq!(
            calibration_word_in(&table, "jev-9.9", &[Threshold::C4Flag]),
            "calibrated"
        );
        assert_eq!(
            calibration_word_in(
                &table,
                "jev-9.9",
                &[Threshold::C4Flag, Threshold::C4SecondOpinion]
            ),
            "uncalibrated",
            "a decision on two floors is calibrated only when both are"
        );
    }

    /// Every value in the table has its provenance, and a value is bound to
    /// the wording it was fitted on: a fitted floor whose question now hashes
    /// differently fails here until it is refitted or removed. The mode
    /// question is built in the shell crate, which checks those entries.
    #[test]
    fn every_calibration_names_its_fit_and_still_matches_its_question() {
        for entry in CALIBRATIONS {
            assert!(entry.n > 0, "{entry:?} has no n");
            assert!(
                entry.fit_from.len() == 10
                    && entry.fit_to.len() == 10
                    && entry.fit_from <= entry.fit_to,
                "{entry:?} needs a fit window"
            );
            assert!(
                (0.0..=1.0).contains(&entry.value),
                "{entry:?} is not a probability"
            );
        }
        let checked: Vec<_> = CALIBRATIONS
            .iter()
            .filter(|entry| entry.threshold.pack() != ThresholdPack::ModeQuestion)
            .copied()
            .collect();
        assert!(
            stale_calibrations(&checked, live_question_hash).is_empty(),
            "a calibrated floor was fitted on wording that changed: refit it or remove it"
        );
    }

    /// The check bites: an entry fitted on another wording is stale, one
    /// fitted on the live wording is not.
    #[test]
    fn a_floor_fitted_on_other_wording_is_reported_stale() {
        let live = live_question_hash(ThresholdPack::DiffReview).expect("built here");
        let entry = |hash: &'static str| Calibration {
            model_id: "jev-9.9",
            threshold: Threshold::C4Flag,
            value: 0.55,
            fit_from: "2026-01-01",
            fit_to: "2026-01-14",
            n: 120,
            question_hash: hash,
        };
        let live: &'static str = Box::leak(live.into_boxed_str());
        assert!(stale_calibrations(&[entry(live)], live_question_hash).is_empty());
        assert_eq!(
            stale_calibrations(&[entry("0123456789ab")], live_question_hash).len(),
            1
        );
    }

    #[test]
    fn the_question_hash_is_twelve_hex_and_tracks_the_wording() {
        let mut questions = BTreeMap::new();
        questions.insert("q".to_owned(), Question::noul("Is it?"));
        let before = question_hash(&questions);
        assert_eq!(before.len(), 12);
        assert!(before.chars().all(|c| c.is_ascii_hexdigit()));
        questions.insert("q".to_owned(), Question::noul("Is it really?"));
        assert_ne!(question_hash(&questions), before);
        assert_eq!(
            live_question_hash(ThresholdPack::PrematureStop),
            live_question_hash(ThresholdPack::PrematureStop)
        );
        assert!(live_question_hash(ThresholdPack::ModeQuestion).is_none());
    }
}
