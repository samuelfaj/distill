//! Area B — effort routing (`todo.md` §2, B1…B3, B6).
//!
//! Routing may only choose **among alternatives the harness already has** (a
//! tool family it already ships, a model tier it already offers, an agent
//! definition the user already defined). Nothing here creates work, forces an
//! upgrade, or spawns a subagent: B6 is a hint, and B2 only ever *downgrades* —
//! except in the auto-effort mode, where the user asked for one effort per model
//! call and the pick is still restricted to the model's own menu.

use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Json, MAX_CHOICE_OPTIONS, Question, QuestionId};

/// Local alias so the family battery reads the same as the other packs.
use crate::jev::types::Question as JevAnswerSetQuestion;

use super::{
    INTENT_MIN_CONFIDENCE, MODEL_TIER_MIN_CONFIDENCE, Pick, Ranked, pick_one, rank_by_noul,
    rank_questions,
};

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
/// B6 — "one tool call is enough" question.
pub const DELEGATION_SINGLE_QUESTION: &str = "fits_single_call";
/// B6 — "independent parts" question.
pub const DELEGATION_PARALLEL_QUESTION: &str = "needs_parallel";

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

// ---------------------------------------------------------------------------
// B2 (local) — can the local model do THIS call?
// ---------------------------------------------------------------------------

/// What the decision is told about the local model. Facts, not marketing: the
/// window it really has and what the owner measured it doing well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModelProfile {
    /// Display name (or endpoint id) of the local model.
    pub name: String,
    /// Context window in tokens, from its catalog entry.
    pub context_window: u64,
    /// The owner's note, verbatim and bounded (may be empty).
    pub notes: String,
}

/// Where one model call should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallRoute {
    /// The local model can do this call on its own — free, so preferred.
    Local,
    /// Keep the session's cloud model for this call.
    Cloud,
}

/// The local model must be **fully** able to do the call, not merely close.
pub const LOCAL_CAPABLE_FLOOR: f64 = 0.70;
/// Probability at which a capability red flag sends the call to the cloud model.
pub const LOCAL_RED_FLAG_FLOOR: f64 = 0.40;

/// Question id of the "can it do this one?" verdict.
pub const LOCAL_CAPABLE_QUESTION: &str = "local_capable";
/// Red flag: more context than the local window holds.
pub const LOCAL_CONTEXT_QUESTION: &str = "needs_bigger_context";
/// Red flag: work that needs a frontier model regardless of size.
pub const LOCAL_FRONTIER_QUESTION: &str = "needs_frontier_reasoning";

/// B2 (local): one capability verdict plus two red flags, in a single request.
///
/// The priority rule is asymmetric on purpose: the local model is free, so it
/// wins whenever it is **fully** capable; the moment a red flag fires — or the
/// verdict is anything but confident — the call goes to the session's model.
pub fn local_model_request(
    profile: &LocalModelProfile,
) -> Result<(Json, BTreeMap<QuestionId, Question>), JevError> {
    if profile.name.trim().is_empty() {
        return Err(JevError::invalid("the local model has no name"));
    }
    let size = "The local model is described in `local_model` (name, context_window_tokens and owner notes).";
    // The unit of judgement is the next single model call (`micro_action` in the
    // state): one step of the work, never the whole task. Without this the model
    // answers "can the small model build this project?" and every step of a big
    // job comes back unsure, even the mechanical ones.
    let step = "Judge ONLY the next single model call described in `micro_action` — one step of the work, \
                not the whole task. A hard project can still have easy next steps (reading a file, \
                fixing a typo, running a test) and an easy project can have a hard one.";
    let mut questions = BTreeMap::new();
    questions.insert(
        LOCAL_CAPABLE_QUESTION.to_owned(),
        Question::noul_with_criteria(
            format!(
                "{step} {size} Can it fully produce that one step — the reasoning and the tool call(s) \
                 it must emit — at the same quality as the session's model, with nothing of that step \
                 left for a stronger model?",
            ),
            "It can do the whole call at the same quality",
            "Part of the job would be lost or done wrong",
        ),
    );
    questions.insert(
        LOCAL_CONTEXT_QUESTION.to_owned(),
        Question::noul_with_criteria(
            format!(
                "{step} {size} Does that one step need more context than that window (including the answer \
                 it must write)?",
            ),
            "It needs more context than the local window holds",
            "It fits comfortably",
        ),
    );
    questions.insert(
        LOCAL_FRONTIER_QUESTION.to_owned(),
        Question::noul_with_criteria(
            format!(
                "{step} {size} Does that one step need frontier-level reasoning — a proof, a subtle \
                 refactor, ambiguous requirements, or exact arithmetic — regardless of how hard the whole \
                 project is?",
            ),
            "It needs frontier-level reasoning",
            "Ordinary capability is enough",
        ),
    );
    Ok((
        serde_json::json!({
            "name": profile.name,
            "context_window_tokens": profile.context_window,
            "notes": profile.notes,
            "execution": "Configured utility model; may be local or remote. Do not assume it is free.",
        }),
        questions,
    ))
}

/// B2 (local): the route for this call, at the pack's default floor.
pub fn compose_local_model(answers: &JevAnswerSet) -> CallRoute {
    compose_local_model_with_floor(answers, LOCAL_CAPABLE_FLOOR)
}

/// B2 (local): the route at an explicit capability floor.
///
/// Deferring (any missing answer, any ambiguous verdict) always means the cloud
/// model — never a silent local run. The floor is a knob because "fully capable"
/// is a judgement the owner can tighten or loosen for their own model.
pub fn compose_local_model_with_floor(answers: &JevAnswerSet, floor: f64) -> CallRoute {
    let capable = match super::noul_of(answers, LOCAL_CAPABLE_QUESTION) {
        Some(probability) => probability,
        None => return CallRoute::Cloud,
    };
    for red_flag in [LOCAL_CONTEXT_QUESTION, LOCAL_FRONTIER_QUESTION] {
        match super::noul_of(answers, red_flag) {
            Some(probability) if probability >= LOCAL_RED_FLAG_FLOOR => return CallRoute::Cloud,
            Some(_) => {}
            None => return CallRoute::Cloud,
        }
    }
    if capable >= floor {
        CallRoute::Local
    } else {
        CallRoute::Cloud
    }
}

/// The probabilities behind one local-model verdict, for the decision record.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LocalVerdict {
    pub capable: Option<f64>,
    pub context: Option<f64>,
    pub frontier: Option<f64>,
}

/// B2 (local): what the battery answered, so a deferral is diagnosable
/// ("wanted local, but only 0.61 capable").
pub fn local_verdict(answers: &JevAnswerSet) -> LocalVerdict {
    LocalVerdict {
        capable: super::noul_of(answers, LOCAL_CAPABLE_QUESTION),
        context: super::noul_of(answers, LOCAL_CONTEXT_QUESTION),
        frontier: super::noul_of(answers, LOCAL_FRONTIER_QUESTION),
    }
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

// ---------------------------------------------------------------------------
// B2 (auto) — the effort for ONE model call
// ---------------------------------------------------------------------------

/// One effort the current model offers, with the description the user would see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortChoice {
    pub id: String,
    pub description: String,
}

/// B2 (auto): one `choice` over the efforts **this model** offers for a single
/// model call. The model's own name is part of the question: "how much thinking
/// does this call need" is answered differently for a small fast model than for
/// a frontier one.
pub fn micro_effort_questions(
    model_name: &str,
    offered: &[EffortChoice],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    micro_effort_questions_for(model_name, offered, MICRO_EFFORT_QUESTION)
}

/// Each candidate gets its own effort question in the same decision request.
pub fn micro_effort_questions_for(
    model_name: &str,
    offered: &[EffortChoice],
    question_id: &str,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if offered.is_empty() {
        return Err(JevError::invalid("the model offers no reasoning efforts"));
    }
    if offered.len() > MAX_CHOICE_OPTIONS {
        return Err(JevError::invalid(format!(
            "{} efforts over the ceiling for one choice question",
            offered.len()
        )));
    }
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    for choice in offered {
        criteria.insert(choice.id.clone(), Json::String(choice.description.clone()));
    }
    let mut questions = BTreeMap::new();
    questions.insert(
        question_id.to_owned(),
        Question::choice(
            format!(
                "Model `{model_name}` is about to make one more model call — the single step described \
                 in `micro_action`. Judge that step only, not the whole task. Which reasoning effort \
                 should THIS one call use? Pick the cheapest effort that still handles the step; do not \
                 pick a stronger setting than the step needs. Minimize total task cost including retries \
                 and recovery, not only this call. Benchmarks are capability evidence, not measured gains \
                 from increasing effort; honor this model's own offered menu."
            ),
            criteria,
        )?,
    );
    Ok(questions)
}

/// Question id of the auto-effort choice.
pub const MICRO_EFFORT_QUESTION: &str = "micro_effort";
pub const UTILITY_EFFORT_QUESTION: &str = "utility_effort";

/// B2: the effort to use for this call, or `None` to keep the session's.
pub fn compose_micro_effort(answers: &JevAnswerSet, offered: &[EffortChoice]) -> Option<String> {
    compose_micro_effort_for(answers, offered, MICRO_EFFORT_QUESTION)
}

/// `offered` is cheapest first. The pick is the cheapest offered level holding
/// at least half of the answer's probability mass (labels outside the menu are
/// ignored), so a split answer lands in the middle instead of on the session's
/// effort. Without a usable distribution the answer's own `choice` applies if
/// offered. No confidence floor.
pub fn compose_micro_effort_for(
    answers: &JevAnswerSet,
    offered: &[EffortChoice],
    question_id: &str,
) -> Option<String> {
    let Some(Answer::Choice {
        choice,
        probabilities,
        ..
    }) = answers.answers.get(question_id)
    else {
        return None;
    };
    let mass = |id: &str| probabilities.get(id).copied().unwrap_or(0.0).max(0.0);
    let total: f64 = offered.iter().map(|c| mass(&c.id)).sum();
    if total > 0.0 {
        let mut cumulative = 0.0;
        for candidate in offered {
            cumulative += mass(&candidate.id);
            if cumulative / total >= 0.5 {
                return Some(candidate.id.clone());
            }
        }
    }
    offered
        .iter()
        .any(|c| &c.id == choice)
        .then(|| choice.clone())
}

/// B2 (auto): the offered efforts, cheapest first, from the model's own menu.
///
/// The order is the cost order the wire uses; the question offers exactly
/// these levels.
pub fn offered_effort_choices(
    menu: &[(String, String)],
    rank: impl Fn(&str) -> u8,
) -> Vec<EffortChoice> {
    let mut choices: Vec<EffortChoice> = menu
        .iter()
        .map(|(id, description)| EffortChoice {
            id: id.clone(),
            description: description.clone(),
        })
        .collect();
    choices.sort_by_key(|choice| (rank(&choice.id), choice.id.clone()));
    choices
}

/// B6: two `noul`s that decide whether delegating is worth suggesting.
pub fn delegation_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = BTreeMap::new();
    questions.insert(
        DELEGATION_SINGLE_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "Can this task be completed with a single tool call (no iteration)?",
            "One call is enough",
            "It needs several steps",
        ),
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
        super::noul_of(answers, DELEGATION_SINGLE_QUESTION),
        super::noul_of(answers, DELEGATION_PARALLEL_QUESTION),
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
pub const CORE_FAMILIES: &[&str] = &["read", "edit", "execute", "interact", "web", "delegate"];

/// Every family the pruner knows about, in a stable order.
pub const TOOL_FAMILIES: &[&str] = &[
    "read", "edit", "execute", "interact", "web", "delegate", "mcp", "media", "schedule",
    "feedback",
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
            | "spawn_subagent"
    ) {
        "delegate"
    } else if matches!(
        lower.as_str(),
        "get_command_or_subagent_output" | "kill_command_or_subagent" | "wait_commands_or_subagents"
    ) {
        "execute"
    } else if matches!(
        lower.as_str(),
        "image_gen" | "image_edit" | "image_to_video" | "reference_to_video"
    ) {
        "media"
    } else if matches!(
        lower.as_str(),
        "scheduler_create" | "scheduler_delete" | "scheduler_list"
    ) {
        "schedule"
    } else if lower == "send_feedback" {
        "feedback"
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
        let instruction = match family {
            "media" => "Does the request ask to generate, edit or animate an image or a video?".to_owned(),
            "schedule" => "Does the request ask to schedule, list or cancel a task that runs later or on a recurring timer?".to_owned(),
            "feedback" => "Is the user giving feedback about this assistant or its tools, or asking to report a problem with them?".to_owned(),
            other => format!("Does this turn need the `{other}` tool family?"),
        };
        questions.insert(
            format!("family_{family}"),
            Question::noul_with_criteria(
                instruction,
                "At least one tool of this family is plausibly needed",
                "Nothing in this turn calls for this family",
            ),
        );
    }
    Ok(questions)
}

/// Keep/drop per prunable family present in `names`, or `None` when any of
/// them went unanswered (the caller then offers everything).
pub fn family_decisions(
    names: &[String],
    answers: &JevAnswerSet,
) -> Option<BTreeMap<&'static str, bool>> {
    let mut decisions: BTreeMap<&'static str, bool> = BTreeMap::new();
    for name in names {
        let Some(family) = tool_family_of(name) else {
            continue;
        };
        if CORE_FAMILIES.contains(&family) || decisions.contains_key(family) {
            continue;
        }
        let probability = super::noul_of(answers, &format!("family_{family}"))?;
        decisions.insert(family, probability >= FAMILY_KEEP_FLOOR);
    }
    (!decisions.is_empty()).then_some(decisions)
}

/// The session's optional tool families. A family joins the tool set the first
/// time a human request needs it and never leaves: the tools array opens the
/// cached prompt prefix, so a family that came and went would re-bill the
/// whole conversation. A family not needed yet is asked about again only when
/// a new human request arrives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFamilySelection {
    included: std::collections::BTreeSet<String>,
    asked_for: Option<String>,
    /// A subagent's ceiling: the optional families its parent session offers.
    /// The parent already judged the human request, so a child never asks
    /// about (or adds) a family the parent left out.
    parent_families: Option<std::collections::BTreeSet<String>>,
}

impl ToolFamilySelection {
    /// Optional families present in `names` that the session has not included.
    /// `mcp_prunable` is false when nothing can find a dropped MCP tool again.
    pub fn pending(&self, names: &[String], mcp_prunable: bool) -> Vec<&'static str> {
        let mut pending = Vec::new();
        for name in names {
            if let Some(family) = tool_family_of(name)
                && !CORE_FAMILIES.contains(&family)
                && (mcp_prunable || family != "mcp")
                && !self.included.contains(family)
                && self.parent_families.as_ref().is_none_or(|parent| parent.contains(family))
                && !pending.contains(&family)
            {
                pending.push(family);
            }
        }
        pending
    }

    /// Bounds a subagent's families by its parent's. The parent's set only
    /// grows, so the child's candidates only grow too.
    pub fn limit_to_parent(&mut self, families: std::collections::BTreeSet<String>) {
        self.parent_families = Some(families);
    }

    /// Whether a human request has been judged, so the optional families this
    /// session offers are a decision rather than the empty default.
    pub fn decided(&self) -> bool {
        self.asked_for.is_some()
    }

    /// Whether `request` still needs an answer about the pending families.
    pub fn should_ask(&self, request: &str) -> bool {
        self.asked_for.as_deref() != Some(request)
    }

    /// Records the answer for `request`. Families judged needed join the set;
    /// with no answer every pending family joins, which is today's full set.
    pub fn record(
        &mut self,
        request: &str,
        pending: &[&'static str],
        decisions: Option<&BTreeMap<&'static str, bool>>,
    ) {
        for family in pending {
            let keep = decisions.is_none_or(|d| d.get(family).copied().unwrap_or(true));
            if keep {
                self.included.insert((*family).to_owned());
            }
        }
        self.asked_for = Some(request.to_owned());
    }

    /// Whether a tool stays in the request: core and unknown tools always do,
    /// optional ones once their family is included.
    pub fn keeps(&self, name: &str, mcp_prunable: bool) -> bool {
        match tool_family_of(name) {
            None => true,
            Some(family) if CORE_FAMILIES.contains(&family) => true,
            Some("mcp") if !mcp_prunable => true,
            Some(family) => self.included.contains(family),
        }
    }
}

/// The pruned tool list, or `None` to keep every tool.
///
/// Unknown tools (no family) are kept, core families are kept, and a family is
/// kept when its probability clears [`FAMILY_KEEP_FLOOR`]. A missing answer for
/// any family returns `None` — the caller then offers everything.
pub fn keep_tools(names: &[String], answers: &JevAnswerSet) -> Option<Vec<String>> {
    let decisions = family_decisions(names, answers)?;
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

    /// Media, scheduling and feedback tools were sent on every request but
    /// almost never used; they are optional families with plain questions.
    #[test]
    fn rarely_used_internal_tools_form_optional_families() {
        let names: Vec<String> = [
            "read_file",
            "spawn_subagent",
            "get_command_or_subagent_output",
            "image_gen",
            "reference_to_video",
            "scheduler_create",
            "send_feedback",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(tool_family_of("spawn_subagent"), Some("delegate"));
        assert_eq!(tool_family_of("get_command_or_subagent_output"), Some("execute"));
        let questions = tool_family_questions(&names).expect("optional families present");
        for family in ["media", "schedule", "feedback"] {
            assert!(questions.contains_key(&format!("family_{family}")), "{family}");
        }
        assert!(!questions.contains_key("family_delegate"));
    }

    /// A family joins when a request needs it and never leaves, so the tools
    /// array (the start of the cached prefix) only changes when it must.
    #[test]
    fn tool_family_selection_only_grows_and_asks_once_per_request() {
        let names: Vec<String> = ["read_file", "image_gen", "scheduler_list", "mcp__acme__deploy"]
            .map(str::to_owned)
            .to_vec();
        let mut selection = ToolFamilySelection::default();
        let pending = selection.pending(&names, false);
        assert_eq!(pending, ["media", "schedule"], "mcp stays core without search_tool");
        let coding = BTreeMap::from([("media", false), ("schedule", false)]);
        selection.record("fix the parser", &pending, Some(&coding));
        assert!(!selection.should_ask("fix the parser"));
        assert!(!selection.keeps("image_gen", false));
        assert!(selection.keeps("read_file", false) && selection.keeps("mcp__acme__deploy", false));

        let icon = BTreeMap::from([("media", true), ("schedule", false)]);
        assert!(selection.should_ask("draw an icon for the app"));
        selection.record("draw an icon for the app", &selection.pending(&names, false), Some(&icon));
        assert!(selection.keeps("image_gen", false));

        let later = BTreeMap::from([("schedule", false)]);
        selection.record("fix the parser again", &selection.pending(&names, false), Some(&later));
        assert!(selection.keeps("image_gen", false), "an included family never leaves");
        assert!(!selection.keeps("scheduler_list", false));

        let mut offline = ToolFamilySelection::default();
        offline.record("anything", &offline.pending(&names, false), None);
        assert!(offline.keeps("image_gen", false) && offline.keeps("scheduler_list", false));
    }

    /// A subagent works for a request its parent already judged: a family the
    /// parent left out is neither asked about nor offered, even when the child
    /// gets no answer, while the parent's own families stay open to the child.
    #[test]
    fn a_child_never_offers_a_family_its_parent_left_out() {
        let names: Vec<String> = ["read_file", "image_gen", "scheduler_list", "send_feedback"]
            .map(str::to_owned)
            .to_vec();
        let mut child = ToolFamilySelection::default();
        child.limit_to_parent(["schedule".to_owned()].into());
        let pending = child.pending(&names, false);
        assert_eq!(pending, ["schedule"]);
        child.record("watch the deploy every hour", &pending, None);
        assert!(child.keeps("scheduler_list", false), "no answer keeps the parent's family");
        assert!(!child.keeps("image_gen", false) && !child.keeps("send_feedback", false));
        assert!(child.keeps("read_file", false));

        child.limit_to_parent(["schedule".to_owned(), "media".to_owned()].into());
        assert_eq!(child.pending(&names, false), ["media"], "the parent's set only grows");
        assert!(!ToolFamilySelection::default().decided() && child.decided());
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
        assert!(!questions.contains_key("family_web"));
        assert!(!questions.contains_key("family_delegate"));
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
        assert!(kept.contains(&"web_search".to_owned()));
        assert!(kept.contains(&"task".to_owned()));
    }

    #[test]
    fn b4_keeps_everything_when_an_answer_is_missing() {
        let names = vec![
            "read_file".to_owned(),
            "web_search".to_owned(),
            "task".to_owned(),
            "mcp__acme__deploy".to_owned(),
        ];
        let partial = answers(vec![("family_web", noul(0.0))]);
        assert!(
            keep_tools(&names, &partial).is_none(),
            "a missing family answer ⇒ offer everything"
        );
        let all_off = answers(vec![
            ("family_web", noul(0.0)),
            ("family_delegate", noul(0.0)),
            ("family_mcp", noul(0.0)),
        ]);
        let kept = keep_tools(&names, &all_off).expect("pruning applies");
        assert_eq!(
            kept,
            vec![
                "read_file".to_owned(),
                "web_search".to_owned(),
                "task".to_owned()
            ]
        );
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

    fn effort(id: &str, description: &str) -> EffortChoice {
        EffortChoice {
            id: id.to_owned(),
            description: description.to_owned(),
        }
    }

    /// The question offers exactly the model's levels. A "keep session effort"
    /// option is gone: its real meaning was the pinned max effort.
    #[test]
    fn b2_auto_question_offers_only_the_models_levels() {
        let offered = vec![
            effort("low", "Faster, lighter reasoning"),
            effort("high", "Heavy reasoning"),
        ];
        let questions =
            micro_effort_questions("DeepSeek V4.1 Flash", &offered).expect("battery builds");
        let Some(Question::Choice {
            instructions,
            criteria,
            ..
        }) = questions.get(MICRO_EFFORT_QUESTION)
        else {
            panic!("auto effort is one choice question");
        };
        let effort_text = instructions.as_str().unwrap_or_default();
        assert!(
            effort_text.contains("DeepSeek V4.1 Flash"),
            "the model's own name is part of the question"
        );
        assert!(
            effort_text.contains("Judge that step only, not the whole task"),
            "the effort is chosen for the step, not the task"
        );
        let keys: Vec<&str> = criteria.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["high", "low"], "no keep option, only the menu");
        assert!(micro_effort_questions("m", &[]).is_err());
    }

    /// A confident answer is simply applied; there is no confidence floor that
    /// sends the call back to the session's effort.
    #[test]
    fn b2_auto_applies_a_confident_answer() {
        let offered = vec![effort("low", ""), effort("high", "")];
        let a = answers(vec![(
            "micro_effort",
            choice("low", 0.92, &[("low", 0.92)]),
        )]);
        assert_eq!(compose_micro_effort(&a, &offered).as_deref(), Some("low"));
    }

    /// A spread answer takes the cheapest level holding half the mass, not a
    /// session fallback: low 0.40 < 0.5, low+medium 0.65 >= 0.5.
    #[test]
    fn b2_auto_spread_answer_takes_the_cheapest_level_holding_half_the_mass() {
        let offered = vec![
            effort("low", ""),
            effort("medium", ""),
            effort("high", ""),
            effort("max", ""),
        ];
        let a = answers(vec![(
            "micro_effort",
            choice(
                "low",
                0.40,
                &[("low", 0.40), ("medium", 0.25), ("high", 0.20), ("max", 0.15)],
            ),
        )]);
        assert_eq!(
            compose_micro_effort(&a, &offered).as_deref(),
            Some("medium")
        );
    }

    /// A label the model does not offer must never be applied or counted.
    #[test]
    fn b2_auto_ignores_labels_outside_the_menu() {
        let offered = vec![effort("low", ""), effort("high", "")];
        let a = answers(vec![(
            "micro_effort",
            choice("quantum", 0.9, &[("quantum", 0.9), ("high", 0.1)]),
        )]);
        assert_eq!(compose_micro_effort(&a, &offered).as_deref(), Some("high"));
    }

    /// Without a distribution the answer's own choice is used only when offered.
    #[test]
    fn b2_auto_without_distribution_uses_the_choice_if_offered() {
        let offered = vec![effort("low", ""), effort("high", "")];
        let offered_choice = answers(vec![("micro_effort", choice("high", 0.9, &[]))]);
        assert_eq!(
            compose_micro_effort(&offered_choice, &offered).as_deref(),
            Some("high")
        );
        let invented = answers(vec![("micro_effort", choice("quantum", 0.9, &[]))]);
        assert_eq!(compose_micro_effort(&invented, &offered), None);
    }

    #[test]
    fn candidate_efforts_are_independent_in_one_battery() {
        const OTHER_EFFORT_QUESTION: &str = "other_effort";
        let main = vec![
            EffortChoice {
                id: "low".into(),
                description: String::new(),
            },
            EffortChoice {
                id: "high".into(),
                description: String::new(),
            },
        ];
        let other = vec![
            EffortChoice {
                id: "medium".into(),
                description: String::new(),
            },
            EffortChoice {
                id: "max".into(),
                description: String::new(),
            },
        ];
        let mut battery = micro_effort_questions("Main", &main).unwrap();
        battery
            .extend(micro_effort_questions_for("Other", &other, OTHER_EFFORT_QUESTION).unwrap());
        assert_eq!(battery.len(), 2);
        let answers = answers(vec![
            (MICRO_EFFORT_QUESTION, choice("low", 0.9, &[("low", 0.9)])),
            (OTHER_EFFORT_QUESTION, choice("max", 0.9, &[("max", 0.9)])),
        ]);
        assert_eq!(
            compose_micro_effort(&answers, &main).as_deref(),
            Some("low")
        );
        assert_eq!(
            compose_micro_effort_for(&answers, &other, OTHER_EFFORT_QUESTION).as_deref(),
            Some("max")
        );
        assert!(compose_micro_effort_for(&answers, &other, MICRO_EFFORT_QUESTION).is_none());
    }

    /// The menu handed to the battery is the model's own, cheapest first.
    #[test]
    fn b2_auto_orders_the_offered_efforts_cheapest_first() {
        let rank = |id: &str| match id {
            "none" => 0,
            "low" => 2,
            "medium" => 3,
            "high" => 4,
            other => panic!("unexpected {other}"),
        };
        let menu = vec![
            ("high".to_owned(), "Heavy reasoning".to_owned()),
            ("low".to_owned(), "Light".to_owned()),
            ("none".to_owned(), "No reasoning".to_owned()),
        ];
        let ordered: Vec<String> = offered_effort_choices(&menu, rank)
            .into_iter()
            .map(|choice| choice.id)
            .collect();
        assert_eq!(ordered, vec!["none", "low", "high"]);
    }

    /// Local-first is asymmetric: the free model wins only when it is **fully**
    /// capable and no red flag fires; every doubt sends the call to the cloud.
    #[test]
    fn b2_local_prefers_the_free_model_only_when_it_is_fully_capable() {
        let profile = LocalModelProfile {
            name: "Qwen3.8-27B-4bit".to_owned(),
            context_window: 32_768,
            notes: "tool calling OK, no reasoning effort".to_owned(),
        };
        let (state, questions) = local_model_request(&profile).expect("battery builds");
        assert_eq!(state["name"], profile.name);
        assert_eq!(state["context_window_tokens"], profile.context_window);
        assert_eq!(state["notes"], profile.notes);
        let wire = serde_json::json!({"state": state, "questions": questions}).to_string();
        assert_eq!(wire.matches(&profile.notes).count(), 1);
        assert_eq!(questions.len(), 3);
        let Some(Question::Noul { instructions, .. }) = questions.get(LOCAL_CAPABLE_QUESTION)
        else {
            panic!("the verdict is a noul");
        };
        let text = instructions.as_str().unwrap_or_default();
        assert!(
            text.contains("`local_model`"),
            "the shared model profile is referenced: {text}"
        );
        assert!(
            text.contains("Judge ONLY the next single model call"),
            "the unit of judgement is the step, not the task: {text}"
        );
        let Some(Question::Noul { instructions, .. }) = questions.get(LOCAL_FRONTIER_QUESTION)
        else {
            panic!("the frontier red flag is a noul");
        };
        assert!(
            instructions
                .as_str()
                .unwrap_or_default()
                .contains("regardless of how hard the whole project is"),
            "the red flag is scoped to the step too"
        );

        let capable = |capable: f64, context: f64, frontier: f64| {
            answers(vec![
                (LOCAL_CAPABLE_QUESTION, noul(capable)),
                (LOCAL_CONTEXT_QUESTION, noul(context)),
                (LOCAL_FRONTIER_QUESTION, noul(frontier)),
            ])
        };
        assert_eq!(
            compose_local_model(&capable(0.9, 0.05, 0.05)),
            CallRoute::Local,
            "capable and no red flag ⇒ free model wins"
        );
        assert_eq!(
            compose_local_model(&capable(0.9, 0.6, 0.05)),
            CallRoute::Cloud,
            "too much context ⇒ cloud"
        );
        assert_eq!(
            compose_local_model(&capable(0.9, 0.05, 0.5)),
            CallRoute::Cloud,
            "frontier reasoning ⇒ cloud, even though it 'could' try"
        );
        assert_eq!(
            compose_local_model(&capable(0.6, 0.05, 0.05)),
            CallRoute::Cloud,
            "close is not full capability"
        );
        // The floor is a knob: at 0.5 the same verdict runs locally.
        assert_eq!(
            compose_local_model_with_floor(&capable(0.6, 0.05, 0.05), 0.5),
            CallRoute::Local
        );
        let verdict = local_verdict(&capable(0.61, 0.12, 0.05));
        assert_eq!(verdict.capable, Some(0.61));
        assert_eq!(verdict.frontier, Some(0.05));
        // Any missing answer is a cloud call: never a silent local run.
        let partial = answers(vec![(LOCAL_CAPABLE_QUESTION, noul(0.95))]);
        assert_eq!(compose_local_model(&partial), CallRoute::Cloud);
        assert_eq!(compose_local_model(&answers(vec![])), CallRoute::Cloud);
        assert!(
            local_model_request(&LocalModelProfile {
                name: "  ".to_owned(),
                context_window: 1,
                notes: String::new(),
            })
            .is_err()
        );
    }

}
