//! Area A — content selection (`todo.md` §2, A1…A6).
//!
//! Every pack here reorders or narrows candidates the **code** already produced
//! (search hits, read results, log lines, memory entries, test names). None of
//! them can introduce a candidate, and every one of them defers to the current
//! path when the answers are missing, wrong-typed or below the floor.

use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Json, Question, QuestionId};

use super::{Pick, Ranked, pick_one, rank_by_noul, rank_questions};

/// A1 — floor for "this is the file the request needs edited".
pub const FILE_TO_EDIT_FLOOR: f64 = 0.35;

/// A3 — probability below which a tool result carries no actionable error.
pub const LOG_ACTIONABLE_FLOOR: f64 = 0.50;
/// A3 — floor for keeping a log/test line as the interesting one.
pub const LOG_LINE_FLOOR: f64 = 0.30;

/// A4 — floor for keeping a web result, and the cap applied after ranking.
pub const WEB_RESULT_FLOOR: f64 = 0.50;
/// A4 — never keep more than this many results, whatever the model says.
pub const WEB_RESULT_CAP: usize = 5;

/// A5 — floor for keeping a memory candidate.
pub const MEMORY_KEEP_FLOOR: f64 = 0.40;

/// A6 — confidence needed before a single test name is chosen.
pub const TEST_TO_RUN_MIN_CONFIDENCE: f64 = 0.60;

/// A1: one `noul` per candidate file, "is this the file the request needs edited?".
pub fn file_to_edit_questions(
    candidates: &[String],
    reasons: &BTreeMap<String, String>,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    rank_questions(
        candidates,
        |id| {
            format!(
                "Does the request most likely need to EDIT the file `{id}`? Context from the search: {}",
                reasons
                    .get(id)
                    .map(String::as_str)
                    .unwrap_or("(no snippet)")
            )
        },
        "This file is the edit target or a necessary part of it",
        "This file is unrelated to the request",
    )
}

/// A1: keeps the files worth opening, best first; keeps everything on doubt.
///
/// The caller then reads only the kept files, in order — the token saving is
/// not opening the rest (plan §1.3.1 P2).
pub fn compose_file_to_edit(
    answers: &crate::jev::types::JevAnswerSet,
    candidates: &[String],
) -> Ranked {
    rank_by_noul(answers, candidates, FILE_TO_EDIT_FLOOR, true)
}

/// A3: the actionable-error screen plus one `choice`-free ranking of lines.
///
/// `lines` are annotated ids the code produced (e.g. `file:12:error[E0308]`), so
/// the state stays small and the model judges text the harness already narrowed.
pub fn log_line_questions(lines: &[String]) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = rank_questions(
        lines,
        |id| {
            format!(
                "Is `{id}` one of the lines that explains the failure (the cause or the first error)?"
            )
        },
        "This line is part of the explanation",
        "This line is noise for the failure",
    )?;
    questions.insert(
        "actionable_error".to_owned(),
        Question::noul_with_criteria(
            "Does this output contain an actionable error the agent should react to?",
            "There is an error worth acting on",
            "No actionable error (success, or pure information)",
        ),
    );
    Ok(questions)
}

/// A3: keeps the explanatory lines, or defers when there is nothing to act on.
pub fn compose_log_lines(answers: &crate::jev::types::JevAnswerSet, lines: &[String]) -> Ranked {
    match super::noul_of(answers, "actionable_error") {
        // No actionable error, or no answer: leave the result untouched.
        Some(p) if p >= LOG_ACTIONABLE_FLOOR => {}
        _ => return Ranked::all(lines),
    }
    rank_by_noul(answers, lines, LOG_LINE_FLOOR, true)
}

/// A4: one `noul` per search result, "would reading this answer the question?".
pub fn web_result_questions(
    results: &[String],
    titles: &BTreeMap<String, String>,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    rank_questions(
        results,
        |id| {
            format!(
                "Would reading result `{id}` answer the question? Title/snippet: {}",
                titles.get(id).map(String::as_str).unwrap_or("(none)")
            )
        },
        "Reading this plausibly answers the question",
        "This result is off-topic",
    )
}

/// A4: keeps the best results, capped, and never drops the whole list.
pub fn compose_web_results(
    answers: &crate::jev::types::JevAnswerSet,
    results: &[String],
) -> Ranked {
    let mut ranked = rank_by_noul(answers, results, WEB_RESULT_FLOOR, true);
    if !ranked.deferred && ranked.keep.len() > WEB_RESULT_CAP {
        ranked.keep.truncate(WEB_RESULT_CAP);
    }
    ranked
}

/// A5: one `noul` per memory candidate, "does this change the work?".
pub fn memory_questions(
    candidates: &[String],
    snippets: &BTreeMap<String, String>,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    rank_questions(
        candidates,
        |id| {
            format!(
                "Is memory `{id}` relevant to the current task, in the sense that it changes what the agent should do? Content: {}",
                snippets.get(id).map(String::as_str).unwrap_or("(none)")
            )
        },
        "This memory changes or constrains the work",
        "This memory is not about this task",
    )
}

/// A5: keeps the relevant memories; short lists pass through untouched.
pub fn compose_memory(answers: &crate::jev::types::JevAnswerSet, candidates: &[String]) -> Ranked {
    rank_by_noul(answers, candidates, MEMORY_KEEP_FLOOR, true)
}

/// A6: a single `choice` over the test names the code shortlisted.
pub fn test_to_run_questions(names: &[String]) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if names.is_empty() {
        return Err(JevError::invalid("no test candidates"));
    }
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    for name in names {
        criteria.insert(name.clone(), Json::Null);
    }
    criteria.insert(
        "run_default_suite".to_owned(),
        Json::String("No single candidate stands out; run the default suite".to_owned()),
    );
    let mut questions = BTreeMap::new();
    questions.insert(
        "test_to_run".to_owned(),
        Question::choice(
            "Which test or test target should be run first for this change?",
            criteria,
        )?,
    );
    Ok(questions)
}

/// A6: the chosen test, or the default suite when it is not confident.
pub fn compose_test_to_run(
    answers: &crate::jev::types::JevAnswerSet,
    names: &[String],
) -> Option<String> {
    let mut allowed: Vec<&str> = names.iter().map(String::as_str).collect();
    allowed.push("run_default_suite");
    let pick: Pick = pick_one(answers, "test_to_run", &allowed, TEST_TO_RUN_MIN_CONFIDENCE);
    match pick.choice.as_deref() {
        Some("run_default_suite") | None => None,
        Some(name) => Some(name.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, choice, ids, noul};

    #[test]
    fn a1_keeps_the_relevant_files_and_defers_on_missing_answers() {
        let candidates = ids(3);
        let answered = answers(vec![
            ("rank_cand-0", noul(0.8)),
            ("rank_cand-1", noul(0.1)),
            ("rank_cand-2", noul(0.5)),
        ]);
        let ranked = compose_file_to_edit(&answered, &candidates);
        assert!(!ranked.is_deferred());
        assert_eq!(ranked.keep, vec!["cand-0".to_owned(), "cand-2".to_owned()]);
        assert_eq!(ranked.confidence, Some(0.5), "lowest kept probability");

        // A partial answer set defers instead of dropping files.
        let partial = answers(vec![("rank_cand-0", noul(0.9))]);
        let deferred = compose_file_to_edit(&partial, &candidates);
        assert!(deferred.is_deferred());
        assert_eq!(deferred.keep, candidates);

        // Nothing clears the floor: keep everything (opening none is worse).
        let nothing = answers(vec![
            ("rank_cand-0", noul(0.05)),
            ("rank_cand-1", noul(0.02)),
            ("rank_cand-2", noul(0.01)),
        ]);
        assert!(compose_file_to_edit(&nothing, &candidates).is_deferred());
    }

    #[test]
    fn a1_wrong_answer_type_defers() {
        let candidates = ids(1);
        let wrong = answers(vec![("rank_cand-0", choice("yes", 0.9, &[("yes", 0.9)]))]);
        assert!(compose_file_to_edit(&wrong, &candidates).is_deferred());
    }

    #[test]
    fn a3_only_recorts_when_there_is_an_actionable_error() {
        let lines = ids(3);
        let with_error = answers(vec![
            ("actionable_error", noul(0.9)),
            ("rank_cand-0", noul(0.9)),
            ("rank_cand-1", noul(0.4)),
            ("rank_cand-2", noul(0.05)),
        ]);
        let ranked = compose_log_lines(&with_error, &lines);
        assert!(!ranked.is_deferred());
        assert_eq!(ranked.keep.len(), 2);
        assert_eq!(ranked.keep[0], "cand-0", "best first");

        let success_run = answers(vec![
            ("actionable_error", noul(0.05)),
            ("rank_cand-0", noul(0.9)),
            ("rank_cand-1", noul(0.9)),
            ("rank_cand-2", noul(0.9)),
        ]);
        assert!(
            compose_log_lines(&success_run, &lines).is_deferred(),
            "a clean run keeps the output as it is"
        );
    }

    #[test]
    fn a4_keeps_the_best_results_and_caps_them() {
        let results = ids(8);
        let mut entries: Vec<(&str, crate::jev::types::Answer)> = Vec::new();
        let owned: Vec<String> = results.clone();
        for (i, id) in owned.iter().enumerate() {
            // Every candidate clears the floor, so the cap is what matters.
            let _ = i;
            entries.push((Box::leak(format!("rank_{id}").into_boxed_str()), noul(0.9)));
        }
        let ranked = compose_web_results(&answers(entries), &results);
        assert_eq!(ranked.keep.len(), WEB_RESULT_CAP);

        let off_topic = answers(
            results
                .iter()
                .map(|id| {
                    (
                        Box::leak(format!("rank_{id}").into_boxed_str()) as &str,
                        noul(0.1),
                    )
                })
                .collect(),
        );
        let deferred = compose_web_results(&off_topic, &results);
        assert!(
            deferred.is_deferred(),
            "dropping every result is not allowed"
        );
    }

    #[test]
    fn a5_keeps_relevant_memories_and_defers_on_doubt() {
        let candidates = ids(2);
        let ranked = compose_memory(
            &answers(vec![("rank_cand-0", noul(0.7)), ("rank_cand-1", noul(0.1))]),
            &candidates,
        );
        assert_eq!(ranked.keep, vec!["cand-0".to_owned()]);
        let doubt = compose_memory(&answers(vec![("rank_cand-0", noul(0.9))]), &candidates);
        assert!(doubt.is_deferred());
    }

    #[test]
    fn a6_picks_one_test_or_falls_back_to_the_default_suite() {
        let names = vec!["test_a".to_owned(), "test_b".to_owned()];
        let questions = test_to_run_questions(&names).expect("battery builds");
        assert!(questions.contains_key("test_to_run"));

        let confident = answers(vec![(
            "test_to_run",
            choice("test_b", 0.85, &[("test_b", 0.85), ("test_a", 0.1)]),
        )]);
        assert_eq!(
            compose_test_to_run(&confident, &names),
            Some("test_b".to_owned())
        );

        let unsure = answers(vec![(
            "test_to_run",
            choice("test_b", 0.4, &[("test_b", 0.4), ("test_a", 0.35)]),
        )]);
        assert_eq!(
            compose_test_to_run(&unsure, &names),
            None,
            "low confidence ⇒ default suite"
        );

        let invalid = answers(vec![(
            "test_to_run",
            choice("test_zzz", 0.99, &[("test_zzz", 0.99)]),
        )]);
        assert_eq!(
            compose_test_to_run(&invalid, &names),
            None,
            "a label outside the candidates is ignored"
        );
    }

    #[test]
    fn a2_line_window_lives_in_the_ladder_pack() {
        // A2 is the existing ladder pack (P2); this pins the id so the catalogue
        // and the code cannot drift apart.
        assert_eq!(
            crate::jev::flags::JevLever::P2ReadShortlist.as_str(),
            "p2_read_shortlist"
        );
    }
}
