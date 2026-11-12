// Modified for Distill by Samuel Fajreldines, 2026.
//! Jev questions asked only to be recorded: they ride a request a live pack
//! already makes, are recorded with a `shadow:` label, and nothing acts on
//! their answers. All of it is off unless `[jev] shadow_packs` is on. Also
//! the check that a calibrated floor of the mode question still matches its
//! wording (the other floors are checked where their questions are built).

use std::collections::BTreeMap;

use distill_workspace::jev::flags::JevLever;
use distill_workspace::jev::{JevAnswerSet, Question};

use super::SessionActor;
use super::jev_auto_mode::{MODE_QUESTION, mode_question};

type ShadowPack = (JevLever, BTreeMap<String, Question>);

impl SessionActor {
    /// In a session pinned to a mode, the mode question as a shadow pack, so
    /// pinned sessions also yield mode decisions to compare with Auto's.
    fn pinned_mode_shadow(&self) -> Option<ShadowPack> {
        if !crate::jev_shadow::packs_on() || self.execution_mode.is_auto() {
            return None;
        }
        mode_question().map(|questions| (JevLever::B8ExecutionMode, questions))
    }

    /// P6's request-start skill ranking. In a pinned session with shadow
    /// packs on, the mode question rides along on the ranking's state (the
    /// request); otherwise this is the plain `ask_item` it always was.
    pub(super) async fn ask_skill_ranking(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Option<JevAnswerSet> {
        let Some(shadow) = self.pinned_mode_shadow() else {
            return crate::jev::ask_item(JevLever::P6SkillSuggestion, state, questions).await;
        };
        let ([answers], mode) = crate::jev::ask_items_with_shadow(
            state,
            [(JevLever::P6SkillSuggestion, Some(questions))],
            Some(shadow),
        )
        .await;
        if let Some(mode) = mode {
            crate::jev::record_item(
                JevLever::B8ExecutionMode,
                &format!(
                    "shadow:mode:{}",
                    mode.choice(MODE_QUESTION).unwrap_or("defer")
                ),
                &format!(
                    "pinned {}; request start, on the skill ranking's state",
                    self.execution_mode.load().label()
                ),
                mode.confidence(MODE_QUESTION),
                Some(&mode),
            );
        }
        answers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_tool_types::ExecutionMode;
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

    fn noul_answers(entries: &[(&str, f64)]) -> JevAnswerSet {
        JevAnswerSet {
            model: "test".to_owned(),
            answers: entries
                .iter()
                .map(|(id, noul)| {
                    (
                        (*id).to_owned(),
                        distill_workspace::jev::Answer::Noul { noul: *noul },
                    )
                })
                .collect(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 3,
        }
    }

    fn mode_answer() -> JevAnswerSet {
        JevAnswerSet {
            answers: [(
                format!("shadow:{MODE_QUESTION}"),
                distill_workspace::jev::Answer::Choice {
                    choice: "ultrafast".to_owned(),
                    probabilities: [("ultrafast".to_owned(), 0.7)].into(),
                    confidence: Some(0.7),
                },
            )]
            .into_iter()
            .chain(noul_answers(&[("0:gate", 0.4)]).answers)
            .collect(),
            ..noul_answers(&[])
        }
    }

    fn skill_pack() -> BTreeMap<String, Question> {
        [("gate".to_owned(), Question::noul("Does a skill apply?"))]
            .into_iter()
            .collect()
    }

    fn reset() {
        crate::jev::clear_test_decision_answers();
        crate::jev::take_test_asked_bodies();
        crate::jev::clear_test_flags();
        crate::jev_shadow::set_test_switches(None, None);
    }

    /// A pinned session's skill ranking carries the mode question only with
    /// shadow packs on; the ranking still gets only its own answers, the mode
    /// pick is recorded as `shadow:mode:<mode>`, and the mode stays pinned.
    #[tokio::test(flavor = "current_thread")]
    async fn a_pinned_session_records_a_shadow_mode_pick_on_the_skill_ranking() {
        tokio::task::LocalSet::new()
            .run_until(crate::jev::with_session_scope("shadow-pinned", async {
                reset();
                crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
                let actor = super::super::support::plain_actor().await;
                actor.execution_mode.store(ExecutionMode::Ultracode);

                crate::jev_shadow::set_test_switches(Some(true), None);
                crate::jev::set_test_decision_answers([Some(mode_answer())]);
                let ranking = actor
                    .ask_skill_ranking(serde_json::json!({"request": "synthetic"}), skill_pack())
                    .await
                    .expect("ranking answers");
                assert_eq!(
                    ranking.answers.len(),
                    1,
                    "the ranking sees only its own answers"
                );
                assert_eq!(ranking.noul("gate"), Some(0.4));
                assert_eq!(
                    crate::jev::take_test_asked_questions(),
                    vec![vec!["0:gate".to_owned(), format!("shadow:{MODE_QUESTION}")]]
                );
                assert!(
                    crate::jev::recorded_decisions_for_test("shadow-pinned").contains(&(
                        "b8_execution_mode".to_owned(),
                        "shadow:mode:ultrafast".to_owned()
                    ))
                );
                assert_eq!(
                    actor.execution_mode.load(),
                    ExecutionMode::Ultracode,
                    "never acted on"
                );

                // Off: the plain request the ranking always sent.
                crate::jev_shadow::set_test_switches(Some(false), None);
                crate::jev::set_test_decision_answers([Some(noul_answers(&[("gate", 0.4)]))]);
                actor
                    .ask_skill_ranking(serde_json::json!({"request": "synthetic"}), skill_pack())
                    .await;
                assert_eq!(
                    crate::jev::take_test_asked_questions(),
                    vec![vec!["gate".to_owned()]]
                );

                // Auto asks the mode question for real; no shadow copy rides along.
                crate::jev_shadow::set_test_switches(Some(true), None);
                actor.execution_mode.store_auto(ExecutionMode::Normal);
                crate::jev::set_test_decision_answers([Some(noul_answers(&[("gate", 0.4)]))]);
                actor
                    .ask_skill_ranking(serde_json::json!({"request": "synthetic"}), skill_pack())
                    .await;
                assert_eq!(
                    crate::jev::take_test_asked_questions(),
                    vec![vec!["gate".to_owned()]]
                );
                reset();
            }))
            .await;
    }
}
