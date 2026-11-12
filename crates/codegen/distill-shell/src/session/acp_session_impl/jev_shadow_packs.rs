// Modified for Distill by Samuel Fajreldines, 2026.
//! The check that a calibrated floor of the mode question still matches its
//! wording (the other floors are checked where their questions are built).

#[cfg(test)]
mod tests {
    use super::super::jev_auto_mode::mode_question;
    use distill_workspace::jev::catalog::thresholds::{
        CALIBRATIONS, ThresholdPack, live_question_hash, question_hash, stale_calibrations,
    };

    /// The mode question is built here, so its calibrated floors are checked
    /// here: an entry fitted on other wording fails until it is refitted.
    #[test]
    fn mode_review_calibrations_still_match_the_mode_question() {
        let live = |pack| match pack {
            ThresholdPack::ModeQuestion => mode_question().map(|q| question_hash(&q)),
            other => live_question_hash(other),
        };
        assert!(
            stale_calibrations(CALIBRATIONS, live).is_empty(),
            "a calibrated floor was fitted on wording that changed: refit it or remove it"
        );
        assert!(
            live(ThresholdPack::ModeQuestion).is_some(),
            "the mode question builds"
        );
    }
}
