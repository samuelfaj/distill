//! Offline replay of Jev requests: the pure half of the ignored replay test
//! (`tests/jev_replay.rs`), which asks Jev again about recorded states.
//!
//! A replay set is JSON lines, one request per line:
//! `{"id", "lever", "state", "questions"?, "pack"?, "pack_levers"?}`. A line
//! either carries the questions as they were sent (what `[jev] capture_states`
//! writes) or names a live catalog `pack`, rebuilt from today's wording. Keys
//! of a batched request keep their `<index>:` prefix, and `pack_levers` names
//! the lever of each index. The committed set holds hand-written states
//! around the benchmark fixtures only; captured sets hold user content and
//! stay local.
//!
//! A replay never decides anything: it says how often two asks disagree, and
//! the same body asked twice gives the noise floor any change must beat.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::catalog::verify;
use super::types::{Answer, JevAnswerSet, Json, NoulCriteria, Question, QuestionId};

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCase {
    /// Defaults to `line-<n>` when the line has none (captured lines).
    #[serde(default)]
    pub id: String,
    /// The lever that owned the request.
    pub lever: String,
    pub state: Json,
    /// The questions as sent; absent when `pack` names a live catalog pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<BTreeMap<QuestionId, Question>>,
    /// A live catalog pack: `diff_review` (state `intent` and `change`),
    /// `final_check` (C1 and C3 batched as the turn end sends them) or
    /// `c4_battery` (UltraCode's shadow review notes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
    /// Lever of each `<index>:` prefix in a batched request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pack_levers: BTreeMap<String, String>,
}

/// Reads a set: one JSON object per line; blank lines and `#` comments skipped.
pub fn load_set(text: &str) -> Result<Vec<ReplayCase>, String> {
    let mut cases = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut case: ReplayCase =
            serde_json::from_str(line).map_err(|error| format!("line {}: {error}", index + 1))?;
        if case.id.is_empty() {
            case.id = format!("line-{}", index + 1);
        }
        if case.questions.is_none() && case.pack.is_none() {
            return Err(format!("line {}: needs `questions` or `pack`", index + 1));
        }
        cases.push(case);
    }
    Ok(cases)
}

/// The questions to send for `case`: as recorded, or the live pack. A live
/// pack fills `pack_levers` for its batched indexes.
pub fn questions_for(case: &mut ReplayCase) -> Result<BTreeMap<QuestionId, Question>, String> {
    if let Some(questions) = &case.questions {
        return Ok(questions.clone());
    }
    match case.pack.as_deref() {
        Some("diff_review") => {
            let text = |key: &str| {
                let value = case.state.get(key).and_then(Json::as_str);
                value.unwrap_or_default().to_owned()
            };
            verify::diff_review_request(&text("intent"), &text("change"))
                .map(|(_, questions)| questions)
                .map_err(|error| format!("{}: {error}", case.id))
        }
        Some("final_check") => {
            let items = ["user request".to_owned()];
            let packs = [
                (
                    "c1_premature_stop",
                    verify::premature_stop_questions(&items),
                ),
                ("c3_completion_check", verify::completion_questions(&items)),
            ];
            let mut questions = BTreeMap::new();
            for (index, (lever, pack)) in packs.into_iter().enumerate() {
                let pack = pack.map_err(|error| format!("{}: {error}", case.id))?;
                case.pack_levers.insert(index.to_string(), lever.to_owned());
                for (id, question) in pack {
                    questions.insert(format!("{index}:{id}"), question);
                }
            }
            Ok(questions)
        }
        Some("c4_battery") => Ok(super::catalog::review_battery::battery_questions()),
        other => Err(format!("{}: unknown pack {other:?}", case.id)),
    }
}

/// The lever a question of `case` belongs to.
pub fn lever_of(case: &ReplayCase, question_id: &str) -> String {
    question_id
        .split_once(':')
        .and_then(|(prefix, _)| case.pack_levers.get(prefix))
        .cloned()
        .unwrap_or_else(|| case.lever.clone())
}

/// A batched request split into its packs, prefixes removed, for the
/// batched-against-solo run. An unbatched request is one pack.
pub fn split_packs(
    questions: &BTreeMap<QuestionId, Question>,
) -> BTreeMap<String, BTreeMap<QuestionId, Question>> {
    let mut packs: BTreeMap<String, BTreeMap<QuestionId, Question>> = BTreeMap::new();
    for (id, question) in questions {
        let (prefix, bare) = match id.split_once(':') {
            Some((prefix, bare)) => (prefix.to_owned(), bare.to_owned()),
            None => (String::new(), id.clone()),
        };
        packs
            .entry(prefix)
            .or_default()
            .insert(bare, question.clone());
    }
    packs
}

/// One question's replacement wording, for a candidate change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    /// Only cases (or batched packs) of this lever are changed.
    pub lever: String,
    /// The question id without any `<index>:` prefix.
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Json>,
    /// Noul: `{"true": …, "false": …}`; choice: label map; score: level array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Json>,
}

impl Swap {
    /// A swap from a JSON file's text, or from the separate values; `None`
    /// when neither names a question.
    pub fn from_parts(
        file: Option<&str>,
        lever: Option<&str>,
        question: Option<&str>,
        instructions: Option<&str>,
        criteria: Option<&str>,
    ) -> Result<Option<Self>, String> {
        if let Some(text) = file {
            return serde_json::from_str(text)
                .map(Some)
                .map_err(|error| format!("swap file: {error}"));
        }
        let (Some(lever), Some(question)) = (lever, question) else {
            return Ok(None);
        };
        let criteria = criteria
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| format!("swap criteria: {error}"))?;
        Ok(Some(Self {
            lever: lever.to_owned(),
            question: question.to_owned(),
            instructions: instructions.map(Json::from),
            criteria,
        }))
    }
}

/// Applies `swap` to the questions of `case`; returns how many it changed.
/// A question of another type than the criteria describe is an error, never
/// a silent skip.
pub fn apply_swap(
    case: &ReplayCase,
    questions: &mut BTreeMap<QuestionId, Question>,
    swap: &Swap,
) -> Result<usize, String> {
    let mut changed = 0;
    for (id, question) in questions.iter_mut() {
        let bare = id.split_once(':').map_or(id.as_str(), |(_, bare)| bare);
        if bare != swap.question || lever_of(case, id) != swap.lever {
            continue;
        }
        let criteria = swap.criteria.clone();
        match question {
            Question::Noul {
                instructions,
                criteria: current,
            } => {
                if let Some(text) = &swap.instructions {
                    *instructions = text.clone();
                }
                if let Some(criteria) = criteria {
                    *current = Some(
                        serde_json::from_value::<NoulCriteria>(criteria)
                            .map_err(|error| format!("{id}: noul criteria: {error}"))?,
                    );
                }
            }
            Question::Choice {
                instructions,
                criteria: current,
            } => {
                if let Some(text) = &swap.instructions {
                    *instructions = text.clone();
                }
                if let Some(criteria) = criteria {
                    *current = serde_json::from_value(criteria)
                        .map_err(|error| format!("{id}: choice criteria: {error}"))?;
                }
            }
            Question::Score {
                instructions,
                criteria: current,
            } => {
                if let Some(text) = &swap.instructions {
                    *instructions = text.clone();
                }
                if let Some(criteria) = criteria {
                    *current = serde_json::from_value(criteria)
                        .map_err(|error| format!("{id}: score criteria: {error}"))?;
                }
            }
        }
        question
            .validate()
            .map_err(|error| format!("{id}: {error}"))?;
        changed += 1;
    }
    Ok(changed)
}

/// Whether two answers to one question would lead a floor to different
/// sides: another choice, a noul on the other side of 0.5, or a score on
/// another level.
pub fn answers_differ(a: &Answer, b: &Answer) -> bool {
    match (a, b) {
        (Answer::Noul { noul: a }, Answer::Noul { noul: b }) => (*a >= 0.5) != (*b >= 0.5),
        (Answer::Choice { choice: a, .. }, Answer::Choice { choice: b, .. }) => a != b,
        (Answer::Score { score: a, .. }, Answer::Score { score: b, .. }) => a.round() != b.round(),
        _ => true,
    }
}

/// Questions compared and questions answered differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub compared: usize,
    pub disagreed: usize,
}

impl Tally {
    pub fn rate(&self) -> f64 {
        if self.compared == 0 {
            0.0
        } else {
            self.disagreed as f64 / self.compared as f64
        }
    }
}

/// Adds the per-lever disagreement between two answer sets for the same
/// questions of `case` to `totals`. Both sets must use the same ids (prefix a
/// solo answer's ids before comparing it with a batched one). A question one
/// side did not answer counts as a disagreement.
pub fn tally(
    case: &ReplayCase,
    a: &JevAnswerSet,
    b: &JevAnswerSet,
    totals: &mut BTreeMap<String, Tally>,
) {
    for (id, answer) in &a.answers {
        let entry = totals.entry(lever_of(case, id)).or_default();
        entry.compared += 1;
        if b.answers
            .get(id)
            .is_none_or(|other| answers_differ(answer, other))
        {
            entry.disagreed += 1;
        }
    }
    for id in b.answers.keys().filter(|id| !a.answers.contains_key(*id)) {
        let entry = totals.entry(lever_of(case, id)).or_default();
        entry.compared += 1;
        entry.disagreed += 1;
    }
}

/// The disagreement table the replay prints.
pub fn format_table(title: &str, totals: &BTreeMap<String, Tally>) -> String {
    let mut out = format!(
        "=== {title} ===\n{:<28} {:>9} {:>10} {:>8}\n",
        "lever", "compared", "disagreed", "rate"
    );
    for (lever, tally) in totals {
        out.push_str(&format!(
            "{lever:<28} {:>9} {:>10} {:>7.1}%\n",
            tally.compared,
            tally.disagreed,
            tally.rate() * 100.0
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(entries: &[(&str, Answer)]) -> JevAnswerSet {
        JevAnswerSet {
            model: "jev-test".to_owned(),
            answers: entries
                .iter()
                .map(|(id, answer)| ((*id).to_owned(), answer.clone()))
                .collect(),
            usage: Default::default(),
            request_id: None,
            latency_ms: 0,
        }
    }

    fn noul(value: f64) -> Answer {
        Answer::Noul { noul: value }
    }

    /// The committed set loads, names only live packs or inline questions,
    /// and every live pack builds from today's catalog.
    #[test]
    fn the_committed_fixture_set_loads_and_builds() {
        let text = include_str!("../../tests/fixtures/jev_replay/fixtures-v1.jsonl");
        let mut cases = load_set(text).expect("set parses");
        assert!((3..=5).contains(&cases.len()), "3 to 5 hand-written states");
        for case in &mut cases {
            let questions = questions_for(case).expect("pack builds");
            assert!(!questions.is_empty(), "{}", case.id);
            for id in questions.keys() {
                assert!(!lever_of(case, id).is_empty());
            }
        }
    }

    #[test]
    fn a_set_line_needs_questions_or_a_pack_and_ids_default_to_the_line() {
        let ok = load_set(
            "\n# comment\n{\"lever\":\"c4_diff_risk\",\"state\":{},\"pack\":\"diff_review\"}\n",
        )
        .expect("valid");
        assert_eq!(ok[0].id, "line-3");
        assert!(load_set("{\"lever\":\"c4_diff_risk\",\"state\":{}}").is_err());
        assert!(load_set("not json").is_err());
    }

    /// The final-check pack is batched as the turn end sends it, so a
    /// question's lever comes from its index.
    #[test]
    fn a_batched_pack_keeps_its_prefixes_and_levers() {
        let mut case = ReplayCase {
            id: "x".into(),
            lever: "c1_premature_stop".into(),
            state: serde_json::json!({}),
            questions: None,
            pack: Some("final_check".into()),
            pack_levers: BTreeMap::new(),
        };
        let questions = questions_for(&mut case).expect("builds");
        assert!(questions.contains_key("0:requested_work_remains"));
        assert!(questions.contains_key("1:anything_left_out"));
        assert_eq!(
            lever_of(&case, "1:anything_left_out"),
            "c3_completion_check"
        );
        assert_eq!(
            lever_of(&case, "requested_work_remains"),
            "c1_premature_stop"
        );
        let packs = split_packs(&questions);
        assert_eq!(packs.len(), 2);
        assert!(packs["1"].contains_key("anything_left_out"));
    }

    #[test]
    fn a_swap_changes_only_its_question_of_its_lever() {
        let mut case = ReplayCase {
            id: "x".into(),
            lever: "c1_premature_stop".into(),
            state: serde_json::json!({}),
            questions: None,
            pack: Some("final_check".into()),
            pack_levers: BTreeMap::new(),
        };
        let mut questions = questions_for(&mut case).expect("builds");
        let before = questions.clone();
        let swap = Swap::from_parts(
            None,
            Some("c3_completion_check"),
            Some("anything_left_out"),
            Some("Was any requested part skipped?"),
            Some(r#"{"true": "Skipped", "false": "Nothing skipped"}"#),
        )
        .expect("parses")
        .expect("names a question");
        assert_eq!(apply_swap(&case, &mut questions, &swap), Ok(1));
        let Question::Noul {
            instructions,
            criteria,
        } = &questions["1:anything_left_out"]
        else {
            panic!("noul expected");
        };
        assert_eq!(instructions, "Was any requested part skipped?");
        assert_eq!(
            criteria.as_ref().and_then(|c| c.is_true.clone()),
            Some(Json::from("Skipped"))
        );
        for (id, question) in &questions {
            if id != "1:anything_left_out" {
                assert_eq!(Some(question), before.get(id), "{id} untouched");
            }
        }
        let other_lever = Swap {
            lever: "c1_premature_stop".into(),
            ..swap.clone()
        };
        assert_eq!(apply_swap(&case, &mut questions, &other_lever), Ok(0));
        let bad = Swap {
            criteria: Some(serde_json::json!("not criteria")),
            ..swap
        };
        assert!(
            apply_swap(&case, &mut questions, &bad).is_err(),
            "wrong criteria shape"
        );
        assert_eq!(Swap::from_parts(None, None, None, None, None), Ok(None));
        let from_file = Swap::from_parts(
            Some(r#"{"lever": "c4_diff_risk", "question": "may_break", "instructions": "x"}"#),
            None,
            None,
            None,
            None,
        )
        .expect("file parses");
        assert_eq!(
            from_file.map(|swap| swap.question),
            Some("may_break".to_owned())
        );
    }

    /// Disagreement is per lever and counts a floor-relevant change only.
    #[test]
    fn disagreement_counts_answers_that_cross_sides() {
        let case = ReplayCase {
            id: "x".into(),
            lever: "c1_premature_stop".into(),
            state: serde_json::json!({}),
            questions: None,
            pack: None,
            pack_levers: [("1".to_owned(), "c3_completion_check".to_owned())].into(),
        };
        let first = answers(&[("0:a", noul(0.7)), ("0:b", noul(0.2)), ("1:c", noul(0.9))]);
        let second = answers(&[("0:a", noul(0.6)), ("0:b", noul(0.55)), ("1:c", noul(0.1))]);
        let mut totals = BTreeMap::new();
        tally(&case, &first, &second, &mut totals);
        assert_eq!(
            totals["c1_premature_stop"],
            Tally {
                compared: 2,
                disagreed: 1
            }
        );
        assert_eq!(
            totals["c3_completion_check"],
            Tally {
                compared: 1,
                disagreed: 1
            }
        );
        assert_eq!(totals["c1_premature_stop"].rate(), 0.5);
        let missing = answers(&[("0:a", noul(0.7))]);
        let mut totals = BTreeMap::new();
        tally(&case, &missing, &first, &mut totals);
        assert_eq!(
            totals["c1_premature_stop"],
            Tally {
                compared: 2,
                disagreed: 1
            }
        );
        assert_eq!(
            totals["c3_completion_check"],
            Tally {
                compared: 1,
                disagreed: 1
            }
        );
        assert!(format_table("same body twice", &totals).contains("c3_completion_check"));

        let choice = |label: &str| Answer::Choice {
            choice: label.to_owned(),
            probabilities: BTreeMap::new(),
            confidence: None,
        };
        assert!(!answers_differ(&choice("normal"), &choice("normal")));
        assert!(answers_differ(&choice("normal"), &choice("ultracode")));
        assert!(answers_differ(&noul(0.5), &choice("normal")));
        assert_eq!(Tally::default().rate(), 0.0);
    }
}
