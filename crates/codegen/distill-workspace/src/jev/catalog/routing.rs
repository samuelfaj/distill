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
use crate::jev::types::{JevAnswerSet, Json, MAX_CHOICE_OPTIONS, Question, QuestionId};

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

/// Confidence needed before the auto-effort decision is applied to a call.
///
/// Calibrated on live answers (2026-09-18, `deepseek-v4.1-flash`, six palette
/// levels): a trivial list request answers `none` at 0.67 and 0.64; a hard
/// request splits `medium` 0.40 / `high` 0.31; a long coding turn kept
/// answering `low` at 0.40-0.43. With six levels the uniform prior is 0.167, so
/// 0.40 is ~2.4x chance: above it the pick is applied, below it the call keeps
/// the session's own effort.
pub const MICRO_EFFORT_MIN_CONFIDENCE: f64 = 0.40;

/// The reasoning-consult floor. Two options put chance at 0.5, so an answer is
/// taken only clearly above it; below it [`reasoning_consult_pick`] is unsure and
/// the harness falls back to the request's complexity.
pub const REASONING_CONSULT_MIN_CONFIDENCE: f64 = 0.55;

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
    if offered.len() + 1 > MAX_CHOICE_OPTIONS {
        return Err(JevError::invalid(format!(
            "{} efforts over the ceiling for one choice question",
            offered.len()
        )));
    }
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    for choice in offered {
        criteria.insert(choice.id.clone(), Json::String(choice.description.clone()));
    }
    criteria.insert(
        MICRO_EFFORT_FALLBACK_LABEL.to_owned(),
        Json::String("Keep this candidate model's configured effort for this call".to_owned()),
    );
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

/// Label used for "keep whatever the session already uses".
pub const MICRO_EFFORT_FALLBACK_LABEL: &str = "keep_session_effort";
/// Question id of the auto-effort choice.
pub const MICRO_EFFORT_QUESTION: &str = "micro_effort";
pub const REASONING_EFFORT_QUESTION: &str = "reasoning_effort";
pub const UTILITY_EFFORT_QUESTION: &str = "utility_effort";

/// What the decision is told about one tier's model. Facts only: what it is
/// called, its id, the window it really has, and the owner's note about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierProfile {
    /// Catalog entry id, for the record.
    pub id: String,
    /// Display name, for the question text.
    pub name: String,
    /// Context window in tokens, from its catalog entry.
    pub context_window: u64,
    /// The owner's note, verbatim and bounded (may be empty).
    pub notes: String,
}

/// Question id of the reasoning consult: can the main model do this request alone?
pub const REASONING_CONSULT_QUESTION: &str = "reasoning_consult";
/// The main model does this step on its own.
pub const MAIN_ALONE_LABEL: &str = "main_alone";
/// The reasoning model plans or reviews this step for the main model first.
pub const CONSULT_REASONING_LABEL: &str = "consult_reasoning";

/// B2: does the main model need the reasoning model to plan THIS request?
///
/// Asked once, at the start of a request. The main model always does the work;
/// the answer only decides whether the reasoning model plans it first. Getting
/// stuck and delivering changes are watched separately by the harness, so this
/// question is about up-front planning only. Only asked when a reasoning model
/// is configured and differs from the main model.
pub fn reasoning_consult_questions(
    main: &TierProfile,
    reasoning: &TierProfile,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if main.id == reasoning.id {
        return Err(JevError::invalid("the reasoning model is the main model"));
    }
    let describe = |role: &str, profile: &TierProfile| {
        let notes = if profile.notes.trim().is_empty() {
            String::new()
        } else {
            format!(" Notes from its owner: {}", profile.notes)
        };
        format!(
            "{role}: `{}` ({} tokens of context).{notes}",
            profile.name, profile.context_window
        )
    };
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    criteria.insert(
        MAIN_ALONE_LABEL.to_owned(),
        Json::String(describe(
            "The main model does this request correctly and completely on its own: a direct \
             answer, a routine or clearly specified change, or a mechanical task",
            main,
        )),
    );
    criteria.insert(
        CONSULT_REASONING_LABEL.to_owned(),
        Json::String(describe(
            "The reasoning model plans this request for the main model first: architecture or \
             design, multi-file or multi-step work, ambiguous requirements, a subtle bug to \
             diagnose, or a result the main model would likely get wrong or leave incomplete",
            reasoning,
        )),
    );
    let mut questions = BTreeMap::new();
    questions.insert(
        REASONING_CONSULT_QUESTION.to_owned(),
        Question::choice(
            format!(
                "A new user request is in `request`. The main model `{}` will do the work \
                 either way. Decide whether it can do this request correctly and completely the \
                 first time on its own, or needs the reasoning model `{}` to plan it first. \
                 The harness separately consults the reasoning model if the main model gets stuck \
                 and reviews changes before delivery, so consult now only when the request needs \
                 an up-front plan: the main model would likely take a wrong approach, miss \
                 requirements, or need retries without one. Judge the work the request needs, not \
                 its wording or length. Minimize total task cost including retries and recovery: \
                 a wrong approach costs more than one consult, and a needless consult costs a \
                 reasoning-model call. Compare benchmarks only within the same source and metric; \
                 missing scores are unknown, never zero. Pricing and endpoint metrics describe \
                 OpenRouter only, not subscriptions or other providers.",
                main.name, reasoning.name
            ),
            criteria,
        )?,
    );
    Ok(questions)
}

/// B2: Jev's confident answer to the consult question: `Some(true)` to plan
/// with the reasoning model, `Some(false)` for the main model alone, `None`
/// when unsure (below the floor, missing, or an unknown label). The harness
/// decides what an unsure answer means.
pub fn reasoning_consult_pick(answers: &JevAnswerSet) -> Option<bool> {
    let pick = pick_one(
        answers,
        REASONING_CONSULT_QUESTION,
        &[MAIN_ALONE_LABEL, CONSULT_REASONING_LABEL],
        REASONING_CONSULT_MIN_CONFIDENCE,
    );
    pick.choice
        .as_deref()
        .map(|choice| choice == CONSULT_REASONING_LABEL)
}

/// B2 — floor for sending one piece of work to the reasoning model in full.
pub const REASONING_BRIEF_FULL_FLOOR: f64 = 0.50;

/// B2: one `noul` per piece of work the reasoning model has not seen yet —
/// does THIS consult need it in full, or does its one-line summary do? `work`
/// pairs each id with that summary; `consult` says what the consult is for.
pub fn reasoning_brief_questions(
    consult: &str,
    work: &[(String, String)],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let ids: Vec<String> = work.iter().map(|(id, _)| id.clone()).collect();
    let summaries: BTreeMap<&str, &str> = work
        .iter()
        .map(|(id, summary)| (id.as_str(), summary.as_str()))
        .collect();
    rank_questions(
        &ids,
        |id| {
            format!(
                "The reasoning model is consulted to {consult}. Does it need work item `{id}` in \
                 full, or is its one-line summary enough? Summary: {}",
                summaries.get(id).copied().unwrap_or("(none)")
            )
        },
        "This consult needs the item's full text",
        "The one-line summary is enough for this consult",
    )
}

/// B2: the work items the reasoning model gets in full. A missing answer
/// defers, and a deferral sends every item in full: a partial answer must not
/// hide evidence from the consult.
pub fn compose_reasoning_brief(answers: &JevAnswerSet, ids: &[String]) -> Ranked {
    rank_by_noul(answers, ids, REASONING_BRIEF_FULL_FLOOR, false)
}

/// B2: the effort to use for this call, or `None` to keep the session's.
pub fn compose_micro_effort(answers: &JevAnswerSet, offered: &[EffortChoice]) -> Option<String> {
    compose_micro_effort_for(answers, offered, MICRO_EFFORT_QUESTION)
}

pub fn compose_micro_effort_for(
    answers: &JevAnswerSet,
    offered: &[EffortChoice],
    question_id: &str,
) -> Option<String> {
    let mut allowed: Vec<&str> = offered.iter().map(|c| c.id.as_str()).collect();
    allowed.push(MICRO_EFFORT_FALLBACK_LABEL);
    let pick = pick_one(answers, question_id, &allowed, MICRO_EFFORT_MIN_CONFIDENCE);
    pick.choice
        .filter(|choice| choice != MICRO_EFFORT_FALLBACK_LABEL)
}

/// B2 (auto): the offered efforts, cheapest first, from the model's own menu.
///
/// The order is the cost order the wire uses; `keep_session_effort` is appended
/// by [`micro_effort_questions`], never here.
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
pub const CORE_FAMILIES: &[&str] = &["read", "edit", "execute", "interact", "web", "delegate"];

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

    /// The reasoning model gets in full only what its consult needs, and the
    /// rest as a line it can still ask about. A missing answer must not hide
    /// evidence, so it sends everything in full; a confident "summary is
    /// enough" for every item sends none in full.
    #[test]
    fn b2_brief_sends_in_full_only_what_the_consult_needs() {
        let work = vec![
            ("w1".to_owned(), "bash cargo test (4000 bytes): test parser ... FAILED".to_owned()),
            ("w2".to_owned(), "read_file README.md (900 bytes): # Distill".to_owned()),
        ];
        let questions =
            reasoning_brief_questions("diagnose why the main model is stuck", &work)
                .expect("battery builds");
        let about_w1 = format!("{:?}", questions.get("rank_w1").expect("one question per item"));
        assert!(about_w1.contains("diagnose why") && about_w1.contains("FAILED"), "{about_w1}");
        let ids: Vec<String> = work.iter().map(|(id, _)| id.clone()).collect();

        let picked = compose_reasoning_brief(
            &answers(vec![("rank_w1", noul(0.9)), ("rank_w2", noul(0.1))]),
            &ids,
        );
        assert_eq!(picked.keep, ["w1"]);
        assert!(!picked.is_deferred());

        let partial = compose_reasoning_brief(&answers(vec![("rank_w1", noul(0.9))]), &ids);
        assert!(partial.is_deferred());
        assert_eq!(partial.keep, ids, "on doubt every item goes in full");

        let none = compose_reasoning_brief(
            &answers(vec![("rank_w1", noul(0.2)), ("rank_w2", noul(0.1))]),
            &ids,
        );
        assert!(none.keep.is_empty() && !none.is_deferred());
    }

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

    /// Auto effort: the pick must be one the model offers, must clear the
    /// confidence floor, and must defer (keep the session's effort) on doubt.
    #[test]
    fn b2_auto_picks_an_offered_effort_and_defers_on_doubt() {
        let offered = vec![
            EffortChoice {
                id: "low".to_owned(),
                description: "Faster, lighter reasoning".to_owned(),
            },
            EffortChoice {
                id: "high".to_owned(),
                description: "Heavy reasoning".to_owned(),
            },
        ];
        let questions =
            micro_effort_questions("DeepSeek V4.1 Flash", &offered).expect("battery builds");
        let Some(Question::Choice { instructions, .. }) = questions.get(MICRO_EFFORT_QUESTION)
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

        let cheap = answers(vec![(
            "micro_effort",
            choice("low", 0.92, &[("low", 0.92)]),
        )]);
        assert_eq!(
            compose_micro_effort(&cheap, &offered).as_deref(),
            Some("low")
        );

        let keep = answers(vec![(
            "micro_effort",
            choice("keep_session_effort", 0.9, &[("keep_session_effort", 0.9)]),
        )]);
        assert_eq!(
            compose_micro_effort(&keep, &offered),
            None,
            "the fallback label keeps the session's effort"
        );

        // Below the floor the call keeps the session's effort.
        let unsure = answers(vec![(
            "micro_effort",
            choice("low", 0.35, &[("low", 0.35)]),
        )]);
        assert_eq!(compose_micro_effort(&unsure, &offered), None);
        // At the floor it applies: with six levels the prior is 0.167, so a
        // 0.40 verdict is a real signal, not noise.
        let clear = answers(vec![(
            "micro_effort",
            choice("low", 0.40, &[("low", 0.40)]),
        )]);
        assert_eq!(
            compose_micro_effort(&clear, &offered).as_deref(),
            Some("low")
        );

        let invented = answers(vec![(
            "micro_effort",
            choice("quantum", 0.99, &[("quantum", 0.99)]),
        )]);
        assert_eq!(
            compose_micro_effort(&invented, &offered),
            None,
            "an effort the model does not offer must never be applied"
        );

        assert!(micro_effort_questions("m", &[]).is_err());
    }

    #[test]
    fn candidate_efforts_are_independent_in_one_battery() {
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
        let reasoning = vec![
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
            .extend(micro_effort_questions_for("Reasoning", &reasoning, REASONING_EFFORT_QUESTION).unwrap());
        assert_eq!(battery.len(), 2);
        let answers = answers(vec![
            (MICRO_EFFORT_QUESTION, choice("low", 0.9, &[("low", 0.9)])),
            (REASONING_EFFORT_QUESTION, choice("max", 0.9, &[("max", 0.9)])),
        ]);
        assert_eq!(
            compose_micro_effort(&answers, &main).as_deref(),
            Some("low")
        );
        assert_eq!(
            compose_micro_effort_for(&answers, &reasoning, REASONING_EFFORT_QUESTION).as_deref(),
            Some("max")
        );
        assert!(compose_micro_effort_for(&answers, &reasoning, MICRO_EFFORT_QUESTION).is_none());
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

    fn profile(id: &str, name: &str) -> TierProfile {
        TierProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            context_window: 272_000,
            notes: String::new(),
        }
    }

    /// The consult question offers exactly the two roles the caller can act on
    /// and names both models, so the decision knows who runs and who advises.
    #[test]
    fn the_consult_question_offers_main_alone_or_consulting_reasoning() {
        let main = profile("codex-luna", "gpt-6-luna");
        let reasoning = profile("codex-sol", "gpt-6-sol");
        let questions = reasoning_consult_questions(&main, &reasoning).expect("distinct models");
        let Question::Choice {
            instructions,
            criteria,
        } = questions
            .get(REASONING_CONSULT_QUESTION)
            .expect("consult question")
        else {
            panic!("expected a choice");
        };
        assert_eq!(
            criteria.keys().map(String::as_str).collect::<Vec<_>>(),
            [CONSULT_REASONING_LABEL, MAIN_ALONE_LABEL]
        );
        let instructions = instructions.as_str().expect("text instructions");
        assert!(instructions.contains("gpt-6-luna") && instructions.contains("gpt-6-sol"));
        assert!(
            instructions.contains("stuck") && instructions.contains("before delivery"),
            "the question must say the other gates exist, or Jev plans defensively: {instructions}"
        );
        assert!(
            criteria[CONSULT_REASONING_LABEL]
                .as_str()
                .expect("consult description")
                .contains("plans this request")
        );
    }

    /// The same model in both roles would consult itself: refuse the question.
    #[test]
    fn the_consult_question_refuses_the_same_model_twice() {
        let one = profile("codex-sol", "gpt-6-sol");
        assert!(reasoning_consult_questions(&one, &one).is_err());
    }

    /// Only a confident, known answer is a pick: an unsure or unknown one is
    /// `None`, so the harness (not a coin flip) decides what doubt means.
    #[test]
    fn only_a_confident_known_answer_is_a_consult_pick() {
        let answers = |choice: &str, confidence: f64| JevAnswerSet {
            model: "test".to_owned(),
            answers: [(
                REASONING_CONSULT_QUESTION.to_owned(),
                crate::jev::types::Answer::Choice {
                    choice: choice.to_owned(),
                    probabilities: std::collections::BTreeMap::new(),
                    confidence: Some(confidence),
                },
            )]
            .into_iter()
            .collect(),
            usage: crate::jev::types::Usage::default(),
            request_id: None,
            latency_ms: 0,
        };
        assert_eq!(reasoning_consult_pick(&answers(CONSULT_REASONING_LABEL, 0.9)), Some(true));
        assert_eq!(reasoning_consult_pick(&answers(MAIN_ALONE_LABEL, 0.9)), Some(false));
        assert_eq!(reasoning_consult_pick(&answers(MAIN_ALONE_LABEL, 0.3)), None);
        assert_eq!(reasoning_consult_pick(&answers(CONSULT_REASONING_LABEL, 0.3)), None);
        assert_eq!(reasoning_consult_pick(&answers("something-else", 0.99)), None);
    }
}
