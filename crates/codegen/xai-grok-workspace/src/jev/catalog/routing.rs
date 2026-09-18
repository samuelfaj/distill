//! Area B — effort routing (`todo.md` §2, B1…B3, B6).
//!
//! Routing may only choose **among alternatives the harness already has** (a
//! tool family it already ships, a model tier it already offers, an agent
//! definition the user already defined). Nothing here creates work, forces an
//! upgrade, or spawns a subagent: B6 is a hint, and B2 only ever *downgrades*.

use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{JevAnswerSet, Json, Question, QuestionId};

/// Local alias so the family battery reads the same as the other packs.
use crate::jev::types::Question as JevAnswerSetQuestion;

use super::{INTENT_MIN_CONFIDENCE, MODEL_TIER_MIN_CONFIDENCE, Pick, pick_one};

/// B1 — intent labels.
pub const INTENT_QUESTION: &str = "intent";
/// B1 — complexity question (a `score` whose top level means "deep work").
pub const COMPLEXITY_QUESTION: &str = "complexity";
/// B1 — labels allowed for the intent choice.
pub const INTENTS: &[&str] = &["question", "edit", "research", "command", "other"];

/// B2 — model tiers, cheapest first. A tier may only move *down*.
pub const MODEL_TIERS: &[&str] = &["cheap", "standard", "deep"];
/// B3 — the "no specific definition fits" label.
pub const DEFAULT_AGENT_LABEL: &str = "use_default_agent";

/// B6 — floor for the delegation hint.
pub const DELEGATION_HINT_FLOOR: f64 = 0.60;

/// B1: intent plus a complexity score, in one request.
pub fn intent_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    criteria.insert(
        "question".to_owned(),
        Json::from("The user asks for an explanation or an answer"),
    );
    criteria.insert(
        "edit".to_owned(),
        Json::from("The user asks for code/config changes"),
    );
    criteria.insert(
        "research".to_owned(),
        Json::from("The user asks to look something up or investigate"),
    );
    criteria.insert(
        "command".to_owned(),
        Json::from("The user asks to run something (build, test, git, deploy)"),
    );
    criteria.insert("other".to_owned(), Json::from("None of the above"));
    let mut questions = BTreeMap::new();
    questions.insert(
        INTENT_QUESTION.to_owned(),
        Question::choice("What is the user asking for in this turn?", criteria)?,
    );
    questions.insert(
        COMPLEXITY_QUESTION.to_owned(),
        Question::score(
            "How much work does this turn need? Judge the request, not its wording.",
            vec![
                Json::from("One step, no searching"),
                Json::from("A few steps in one or two files"),
                Json::from("Multi-file work with investigation"),
                Json::from("Large change needing planning and many tool calls"),
            ],
        )?,
    );
    Ok(questions)
}

/// B1: the intent, or a deferral (`intent = None`).
pub fn compose_intent(answers: &JevAnswerSet) -> Pick {
    pick_one(answers, INTENT_QUESTION, INTENTS, INTENT_MIN_CONFIDENCE)
}

/// B1: normalized complexity (0..=1), when the answer is usable.
pub fn compose_complexity(answers: &JevAnswerSet) -> Option<f64> {
    super::score_of(answers, COMPLEXITY_QUESTION)
}

/// B2: one choice over the tiers the harness already offers.
pub fn model_tier_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    criteria.insert(
        "cheap".to_owned(),
        Json::from("Routine: a smaller/cheaper model handles it"),
    );
    criteria.insert(
        "standard".to_owned(),
        Json::from("Normal work: the default model"),
    );
    criteria.insert(
        "deep".to_owned(),
        Json::from("Hard work: the strongest/longest setting"),
    );
    let mut questions = BTreeMap::new();
    questions.insert(
        "model_tier".to_owned(),
        Question::choice(
            "Which model tier does this turn need? Do not pick a stronger tier than necessary.",
            criteria,
        )?,
    );
    Ok(questions)
}

/// B2: the tier to apply. **Only downgrades** are returned unless
/// `allow_upgrade` is set, because raising cost is never a Jev decision.
pub fn compose_model_tier(answers: &JevAnswerSet, allow_upgrade: bool) -> Pick {
    let pick = pick_one(
        answers,
        "model_tier",
        MODEL_TIERS,
        MODEL_TIER_MIN_CONFIDENCE,
    );
    if !allow_upgrade && pick.choice.as_deref() == Some("deep") {
        return Pick::defer();
    }
    pick
}

/// B3: one choice over the agent definitions the session already has.
pub fn subagent_type_questions(
    definitions: &[String],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if definitions.is_empty() {
        return Err(JevError::invalid("no agent definitions to choose from"));
    }
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    for definition in definitions {
        criteria.insert(definition.clone(), Json::Null);
    }
    criteria.insert(
        DEFAULT_AGENT_LABEL.to_owned(),
        Json::from("No specific definition fits; use the default agent"),
    );
    let mut questions = BTreeMap::new();
    questions.insert(
        "subagent_type".to_owned(),
        Question::choice(
            "Which existing agent definition should handle this task? Prefer the default when unsure.",
            criteria,
        )?,
    );
    Ok(questions)
}

/// B3: the definition to use, or `None` for the default agent.
pub fn compose_subagent_type(answers: &JevAnswerSet, definitions: &[String]) -> Option<String> {
    let mut allowed: Vec<&str> = definitions.iter().map(String::as_str).collect();
    allowed.push(DEFAULT_AGENT_LABEL);
    let pick = pick_one(answers, "subagent_type", &allowed, 0.70);
    match pick.choice.as_deref() {
        Some(DEFAULT_AGENT_LABEL) | None => None,
        Some(name) => Some(name.to_owned()),
    }
}

/// B6: two `noul`s that decide whether delegating is worth suggesting.
pub fn delegation_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "fits_single_call".to_owned(),
        Question::noul_with_criteria(
            "Can this task be completed with a single tool call (no iteration)?",
            "One call is enough",
            "It needs several steps",
        ),
    );
    questions.insert(
        "needs_parallel".to_owned(),
        Question::noul_with_criteria(
            "Does this task contain independent parts that could run in parallel?",
            "Parts are independent",
            "The work is sequential",
        ),
    );
    Ok(questions)
}

/// B6: whether to *suggest* delegating (never to spawn anything).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelegationHint {
    pub suggest_delegation: bool,
    pub deferred: bool,
}

/// B6: suggests delegation only when the work is neither single-call nor pure
/// sequential; defers on any missing answer.
pub fn compose_delegation(answers: &JevAnswerSet) -> DelegationHint {
    let (Some(single), Some(parallel)) = (
        super::noul_of(answers, "fits_single_call"),
        super::noul_of(answers, "needs_parallel"),
    ) else {
        return DelegationHint {
            suggest_delegation: false,
            deferred: true,
        };
    };
    DelegationHint {
        suggest_delegation: single < (1.0 - DELEGATION_HINT_FLOOR)
            && parallel >= DELEGATION_HINT_FLOOR,
        deferred: false,
    }
}

// ---------------------------------------------------------------------------
// B4/P1 — tool families (the pure half; the shell maps definitions to names)
// ---------------------------------------------------------------------------

/// Families whose tools are always offered: the agent cannot work without them.
pub const CORE_FAMILIES: &[&str] = &["read", "edit", "execute", "interact"];

/// Every family the pruner knows about, in a stable order.
pub const TOOL_FAMILIES: &[&str] = &[
    "read", "edit", "execute", "interact", "web", "delegate", "mcp",
];

/// Probability at or above which a non-core family survives the pruning.
pub const FAMILY_KEEP_FLOOR: f64 = 0.25;

/// Classifies one tool name into a family.
///
/// `None` means "not a tool this pruner understands" — those are always kept, so
/// a new tool can never be pruned away by accident.
pub fn tool_family_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let family = if lower.starts_with("mcp__") || lower.starts_with("mcp_") {
        "mcp"
    } else if matches!(
        lower.as_str(),
        "read_file" | "list_dir" | "search" | "grep" | "glob" | "search_files"
    ) {
        "read"
    } else if matches!(
        lower.as_str(),
        "search_replace" | "write" | "edit" | "apply_patch" | "create_file"
    ) {
        "edit"
    } else if matches!(
        lower.as_str(),
        "bash"
            | "shell"
            | "run_terminal_command"
            | "kill_task"
            | "kill_command"
            | "get_command_output"
            | "get_terminal_command_output"
            | "wait_commands"
    ) {
        "execute"
    } else if matches!(
        lower.as_str(),
        "web_search" | "web_fetch" | "fetch_url" | "browse"
    ) {
        "web"
    } else if matches!(
        lower.as_str(),
        "task"
            | "task_output"
            | "get_task_output"
            | "get_task_or_subagent_output"
            | "wait_tasks"
            | "send_subagent_message"
            | "spawn_agent"
    ) {
        "delegate"
    } else if matches!(
        lower.as_str(),
        "ask_user_question" | "todo_write" | "update_plan" | "exit_plan_mode" | "enter_plan_mode"
    ) {
        "interact"
    } else {
        return None;
    };
    Some(family)
}

/// One `noul` per non-core family present in `names`: "does this turn need it?".
pub fn tool_family_questions(
    names: &[String],
) -> Result<BTreeMap<QuestionId, JevAnswerSetQuestion>, JevError> {
    let mut families: Vec<&'static str> = Vec::new();
    for name in names {
        if let Some(family) = tool_family_of(name)
            && !CORE_FAMILIES.contains(&family)
            && !families.contains(&family)
        {
            families.push(family);
        }
    }
    if families.is_empty() {
        return Err(JevError::invalid("no prunable families in this tool set"));
    }
    let mut questions = BTreeMap::new();
    for family in families {
        questions.insert(
            format!("family_{family}"),
            Question::noul_with_criteria(
                format!("Does this turn need the `{family}` tool family?"),
                "At least one tool of this family is plausibly needed",
                "Nothing in this turn calls for this family",
            ),
        );
    }
    Ok(questions)
}

/// The pruned tool list, or `None` to keep every tool.
///
/// Unknown tools (no family) are kept, core families are kept, and a family is
/// kept when its probability clears [`FAMILY_KEEP_FLOOR`]. A missing answer for
/// any family returns `None` — the caller then offers everything.
pub fn keep_tools(names: &[String], answers: &JevAnswerSet) -> Option<Vec<String>> {
    let mut decisions: BTreeMap<&'static str, bool> = BTreeMap::new();
    for name in names {
        let Some(family) = tool_family_of(name) else {
            continue;
        };
        if CORE_FAMILIES.contains(&family) || decisions.contains_key(family) {
            continue;
        }
        let Some(probability) = super::noul_of(answers, &format!("family_{family}")) else {
            return None;
        };
        decisions.insert(family, probability >= FAMILY_KEEP_FLOOR);
    }
    if decisions.is_empty() {
        return None;
    }
    Some(
        names
            .iter()
            .filter(|name| match tool_family_of(name) {
                None => true,
                Some(family) => {
                    CORE_FAMILIES.contains(&family)
                        || decisions.get(family).copied().unwrap_or(true)
                }
            })
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, choice, noul, score};

    #[test]
    fn b1_reads_intent_and_complexity_and_defers_when_unsure() {
        let questions = intent_questions().expect("battery builds");
        assert!(questions.contains_key(INTENT_QUESTION));
        assert!(questions.contains_key(COMPLEXITY_QUESTION));

        let confident = answers(vec![
            ("intent", choice("edit", 0.9, &[("edit", 0.9)])),
            ("complexity", score(2.0)),
        ]);
        assert_eq!(compose_intent(&confident).choice.as_deref(), Some("edit"));
        // The answer's own legend is the scale: 2 on a 3-level rubric is the top.
        assert_eq!(compose_complexity(&confident), Some(1.0));

        let unsure = answers(vec![("intent", choice("edit", 0.4, &[("edit", 0.4)]))]);
        assert!(compose_intent(&unsure).deferred);
        assert_eq!(
            compose_complexity(&unsure),
            None,
            "no score answer ⇒ no routing"
        );
    }

    #[test]
    fn b2_never_upgrades_tier_and_defers_on_doubt() {
        let deep = answers(vec![(
            "model_tier",
            choice("deep", 0.95, &[("deep", 0.95)]),
        )]);
        assert!(
            compose_model_tier(&deep, false).deferred,
            "raising cost is never a Jev decision"
        );
        assert_eq!(
            compose_model_tier(&deep, true).choice.as_deref(),
            Some("deep"),
            "explicit opt-in allows it"
        );
        let cheap = answers(vec![(
            "model_tier",
            choice("cheap", 0.9, &[("cheap", 0.9)]),
        )]);
        assert_eq!(
            compose_model_tier(&cheap, false).choice.as_deref(),
            Some("cheap")
        );
        let unsure = answers(vec![(
            "model_tier",
            choice("cheap", 0.5, &[("cheap", 0.5)]),
        )]);
        assert!(compose_model_tier(&unsure, false).deferred);
    }

    #[test]
    fn b3_picks_an_existing_definition_or_the_default() {
        let definitions = vec!["explore".to_owned(), "plan".to_owned()];
        assert!(subagent_type_questions(&definitions).is_ok());
        let chosen = answers(vec![(
            "subagent_type",
            choice("plan", 0.8, &[("plan", 0.8)]),
        )]);
        assert_eq!(
            compose_subagent_type(&chosen, &definitions),
            Some("plan".to_owned())
        );
        let none = answers(vec![(
            "subagent_type",
            choice(DEFAULT_AGENT_LABEL, 0.9, &[(DEFAULT_AGENT_LABEL, 0.9)]),
        )]);
        assert_eq!(compose_subagent_type(&none, &definitions), None);
        let invented = answers(vec![(
            "subagent_type",
            choice("made-up", 0.99, &[("made-up", 0.99)]),
        )]);
        assert_eq!(compose_subagent_type(&invented, &definitions), None);
    }

    #[test]
    fn b4_prunes_only_non_core_families_and_keeps_unknown_tools() {
        let names = vec![
            "read_file".to_owned(),
            "search_replace".to_owned(),
            "bash".to_owned(),
            "web_search".to_owned(),
            "task".to_owned(),
            "mcp__acme__deploy".to_owned(),
            "brand_new_tool".to_owned(),
        ];
        let questions = tool_family_questions(&names).expect("families detected");
        // web, delegate and mcp are prunable; the core four are not asked about.
        assert!(questions.contains_key("family_web"));
        assert!(questions.contains_key("family_delegate"));
        assert!(questions.contains_key("family_mcp"));
        assert!(!questions.contains_key("family_read"));

        // Web and delegate are not needed; mcp is unknown to the model here.
        let answers = answers(vec![
            ("family_web", noul(0.05)),
            ("family_delegate", noul(0.10)),
            ("family_mcp", noul(0.90)),
        ]);
        let kept = keep_tools(&names, &answers).expect("pruning applies");
        assert!(kept.contains(&"read_file".to_owned()));
        assert!(kept.contains(&"bash".to_owned()));
        assert!(kept.contains(&"mcp__acme__deploy".to_owned()));
        assert!(
            kept.contains(&"brand_new_tool".to_owned()),
            "an unknown tool is never pruned"
        );
        assert!(!kept.contains(&"web_search".to_owned()));
        assert!(!kept.contains(&"task".to_owned()));
    }

    #[test]
    fn b4_keeps_everything_when_an_answer_is_missing() {
        let names = vec![
            "read_file".to_owned(),
            "web_search".to_owned(),
            "task".to_owned(),
        ];
        let partial = answers(vec![("family_web", noul(0.0))]);
        assert!(
            keep_tools(&names, &partial).is_none(),
            "a missing family answer ⇒ offer everything"
        );
        let all_off = answers(vec![
            ("family_web", noul(0.0)),
            ("family_delegate", noul(0.0)),
        ]);
        let kept = keep_tools(&names, &all_off).expect("pruning applies");
        assert_eq!(kept, vec!["read_file".to_owned()]);
        // A tool set with nothing to prune reports no questions.
        assert!(tool_family_questions(&["read_file".to_owned()]).is_err());
    }

    #[test]
    fn b6_hints_delegation_only_for_parallel_multi_step_work() {
        let questions = delegation_questions().expect("battery builds");
        assert_eq!(questions.len(), 2);

        let parallel = answers(vec![
            ("fits_single_call", noul(0.1)),
            ("needs_parallel", noul(0.85)),
        ]);
        assert!(compose_delegation(&parallel).suggest_delegation);

        let sequential = answers(vec![
            ("fits_single_call", noul(0.1)),
            ("needs_parallel", noul(0.2)),
        ]);
        assert!(!compose_delegation(&sequential).suggest_delegation);

        let one_call = answers(vec![
            ("fits_single_call", noul(0.95)),
            ("needs_parallel", noul(0.9)),
        ]);
        assert!(!compose_delegation(&one_call).suggest_delegation);

        let partial = answers(vec![("fits_single_call", noul(0.1))]);
        assert!(
            compose_delegation(&partial).deferred,
            "missing answer ⇒ no hint"
        );
    }
}
