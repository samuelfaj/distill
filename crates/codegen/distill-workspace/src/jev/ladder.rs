//! The token-saving ladder (plan §1.3.1): P1 tool families, P2 read shortlist,
//! P3 compaction recorte, P6 skill suggestion — plus the
//! P4 note and the per-lever measurement ledger.
//!
//! Two rules hold everywhere in this module:
//! 1. **Code narrows before Jev sees anything** (item 97/104/108): every lever
//!    receives a candidate list the harness already computed, never a whole file
//!    or a whole catalog as free text.
//! 2. **Missing or uncertain answers fall back to the current behaviour** — never
//!    to a smaller answer set. Dropping a tool or a line is a downgrade, so the
//!    conservative direction is always "keep everything" / "read everything".

use std::collections::{BTreeMap, BTreeSet};

use crate::jev::error::JevError;
use crate::jev::policy::{DecisionSink, JevDecision, record_for};
use crate::jev::types::{Answer, JevAnswerSet, Json, MAX_CHOICE_OPTIONS, Question, QuestionId};

// ---------------------------------------------------------------------------
// Shared: token estimation and the measurement ledger
// ---------------------------------------------------------------------------

/// Rough token estimate (4 chars/token). Structural only; the measured numbers
/// that matter are the API's own `usage` counters, captured by the sink.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// One "before/after" token measurement for a lever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeverMeasurement {
    pub lever: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

impl LeverMeasurement {
    /// Tokens the lever avoided (negative when the lever costs more).
    pub fn saved(&self) -> i64 {
        self.tokens_before as i64 - self.tokens_after as i64
    }
}

/// Collects per-lever measurements for the final report.
#[derive(Debug, Default, Clone)]
pub struct MeasurementLedger {
    entries: Vec<LeverMeasurement>,
}

impl MeasurementLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, measurement: LeverMeasurement) {
        self.entries.push(measurement);
    }

    pub fn entries(&self) -> &[LeverMeasurement] {
        &self.entries
    }

    /// Sum of `saved()` across every lever.
    pub fn total_saved(&self) -> i64 {
        self.entries.iter().map(LeverMeasurement::saved).sum()
    }
}

/// P4 is measured as money, not tokens: it changes *which* model answers a turn,
/// so the token count of the turn is unchanged (plan §1.3.1 row P4).
pub const P4_DOC: &str = "P4 (model/effort routing) is a money lever: it changes which model answers the turn, not how many tokens the turn needs. Recorded as cost, not tokens.";

// ---------------------------------------------------------------------------
// P1 — per-turn tool families
// ---------------------------------------------------------------------------

/// A family of tools the harness can withhold for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFamily {
    pub id: String,
    pub description: String,
    pub tools: Vec<String>,
}

impl ToolFamily {
    pub fn new(id: impl Into<String>, description: impl Into<String>, tools: &[&str]) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        }
    }
}

/// Result of a P1 decision.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSelection {
    /// Family ids to keep for this turn.
    pub families: Vec<String>,
    /// Tool names to offer the model (core families always included).
    pub tools: Vec<String>,
    /// True when the conservative path was taken: every family is kept.
    pub kept_everything: bool,
    /// Lowest probability among the kept decisions, when any were made.
    pub confidence: Option<f64>,
}

/// Probability at or above which a family is kept (item 15: the extra per-family
/// questions are nearly free, and keeping a family is cheaper than missing a tool).
pub const P1_KEEP_AT_LEAST: f64 = 0.25;

/// Builds one `noul` per family: "does this turn need the <family> family?".
/// Families go into the *questions* (cheap), never into the state (item 6).
pub fn tool_family_questions(
    families: &[ToolFamily],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if families.is_empty() {
        return Err(JevError::invalid("no tool families to ask about"));
    }
    let mut questions = BTreeMap::new();
    for family in families {
        questions.insert(
            format!("family_{}", family.id),
            Question::noul_with_criteria(
                format!(
                    "Does this turn need the `{}` tool family? The family covers: {}",
                    family.id, family.description
                ),
                "At least one tool of this family is plausibly needed",
                "Nothing in this turn calls for this family",
            ),
        );
    }
    Ok(questions)
}

/// Composes the family answers into a tool subset. `always_keep` families are
/// added unconditionally (the harness's mandatory core).
pub fn select_tool_families(
    answers: Option<&JevAnswerSet>,
    families: &[ToolFamily],
    always_keep: &[&str],
) -> ToolSelection {
    let all_tools = || {
        families
            .iter()
            .flat_map(|f| f.tools.iter().cloned())
            .collect::<Vec<_>>()
    };

    let Some(answers) = answers else {
        return ToolSelection {
            families: families.iter().map(|f| f.id.clone()).collect(),
            tools: all_tools(),
            kept_everything: true,
            confidence: None,
        };
    };

    let mut kept: BTreeSet<String> = always_keep.iter().map(|f| (*f).to_owned()).collect();
    let mut lowest: Option<f64> = None;
    let mut made_a_decision = false;
    for family in families {
        if kept.contains(&family.id) {
            continue;
        }
        let id = format!("family_{}", family.id);
        match answers.noul(&id) {
            Some(probability) => {
                made_a_decision = true;
                lowest = Some(lowest.map_or(probability, |l: f64| l.min(probability)));
                if probability >= P1_KEEP_AT_LEAST {
                    kept.insert(family.id.clone());
                }
            }
            // An unanswered family means we cannot narrow safely: keep everything.
            None => {
                return ToolSelection {
                    families: families.iter().map(|f| f.id.clone()).collect(),
                    tools: all_tools(),
                    kept_everything: true,
                    confidence: None,
                };
            }
        }
    }

    let tools: Vec<String> = families
        .iter()
        .filter(|f| kept.contains(&f.id))
        .flat_map(|f| f.tools.iter().cloned())
        .collect();
    ToolSelection {
        families: kept.into_iter().collect(),
        tools,
        kept_everything: false,
        confidence: if made_a_decision { lowest } else { None },
    }
}

/// Token measurement for P1: every tool schema versus the selected subset.
pub fn measure_tool_selection(
    schemas: &BTreeMap<String, String>,
    selection: &ToolSelection,
) -> LeverMeasurement {
    let before: u64 = schemas.values().map(|s| estimate_tokens(s)).sum();
    let after: u64 = schemas
        .iter()
        .filter(|(name, _)| selection.tools.iter().any(|t| t == *name))
        .map(|(_, schema)| estimate_tokens(schema))
        .sum();
    LeverMeasurement {
        lever: "p1_tool_family".to_owned(),
        tokens_before: before,
        tokens_after: after,
    }
}

// ---------------------------------------------------------------------------
// P2 — read a shortlist instead of a whole file
// ---------------------------------------------------------------------------

/// One candidate line the harness already shortlisted in code (grep/BM25/window).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCandidate {
    pub line: usize,
    pub text: String,
}

/// Result of a P2 decision.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortlistOutcome {
    /// Line numbers to read, best first.
    pub selected: Vec<usize>,
    /// Probability that the shortlist contains the answer at all.
    pub exists: Option<f64>,
    /// True when the model says nothing here answers the question: the caller
    /// must fall back to a wider read instead of returning an empty result.
    pub no_answer: bool,
}

/// Existence probability below which the caller widens the search (item 98).
pub const P2_EXISTS_FLOOR: f64 = 0.35;

/// Whole paragraphs, including fenced code, with their original starting line.
/// No prefix window: either the full supplied document fits or P2 abstains.
pub fn paragraph_candidates(text: &str) -> Vec<LineCandidate> {
    let mut blocks = Vec::new();
    let mut block = String::new();
    let mut start = 1;
    let mut fence: Option<(char, usize)> = None;
    for (index, line) in text.lines().enumerate() {
        if block.is_empty() {
            start = index + 1;
        }
        let trimmed = line.trim_start();
        if let Some(marker @ ('`' | '~')) = trimmed.chars().next() {
            let width = trimmed.chars().take_while(|c| *c == marker).count();
            if width >= 3 {
                if fence.is_some_and(|(opened, size)| opened == marker && width >= size)
                    && trimmed[width..].trim().is_empty()
                {
                    fence = None;
                } else if fence.is_none() {
                    fence = Some((marker, width));
                }
            }
        }
        block.push_str(line);
        block.push('\n');
        if line.trim().is_empty() && fence.is_none() && !block.trim().is_empty() {
            blocks.push(LineCandidate {
                line: start,
                text: std::mem::take(&mut block),
            });
        }
    }
    if !block.is_empty() {
        blocks.push(LineCandidate {
            line: start,
            text: block,
        });
    }
    blocks
}

/// Independent relevance questions, not probabilities of being the single best line.
pub fn shortlist_request(
    candidates: &[LineCandidate],
    request: &str,
) -> Result<(Json, BTreeMap<QuestionId, Question>), JevError> {
    if request.trim().is_empty() || candidates.len() < 3 || candidates.len() > 64 {
        return Err(JevError::invalid(
            "a shortlist needs a query and 3..64 complete blocks",
        ));
    }
    let state = serde_json::json!({
        "request": request,
        "candidates": candidates.iter().map(|c| (c.line.to_string(), c.text.clone())).collect::<BTreeMap<_, _>>(),
    });
    if state.to_string().len() > 16 * 1024 {
        return Err(JevError::invalid(
            "full candidate evidence exceeds the shortlist budget",
        ));
    }
    let mut questions = BTreeMap::new();
    questions.insert(
        "answer_exists".into(),
        Question::noul("Does the supplied document contain the answer to request?"),
    );
    questions.insert("narrow_safe".into(), Question::noul_with_criteria(
        "Is request a selective lookup where unrelated paragraphs may be omitted?",
        "Only specific information is requested",
        "A complete read, review, transformation, exact output or broad understanding is required"));
    for candidate in candidates {
        questions.insert(format!("keep_{}", candidate.line), Question::noul_with_criteria(
            format!("Is candidates[{}] relevant to request, or needed to interpret the answer or preserve a constraint?", candidate.line),
            "Relevant evidence, context, definition or constraint",
            "Unrelated to the requested lookup"));
    }
    Ok((state, questions))
}

/// Keep every uncertain block. Missing answers and non-selective requests do not narrow.
pub fn compose_shortlist(answers: &JevAnswerSet, candidates: &[LineCandidate]) -> ShortlistOutcome {
    let exists = answers.noul("answer_exists");
    let unchanged = || ShortlistOutcome {
        selected: Vec::new(),
        exists,
        no_answer: true,
    };
    if !exists.is_some_and(|p| p >= P2_EXISTS_FLOOR)
        || !answers.noul("narrow_safe").is_some_and(|p| p >= 0.95)
    {
        return unchanged();
    }
    let mut selected = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(p) = answers.noul(&format!("keep_{}", candidate.line)) else {
            return unchanged();
        };
        if !(0.0..=1.0).contains(&p) {
            return unchanged();
        }
        if p > 0.05 || index == 0 || index + 1 == candidates.len() {
            selected.push(candidate.line);
        }
    }
    ShortlistOutcome {
        selected,
        exists,
        no_answer: false,
    }
}

/// Token measurement for P2: the whole document versus the selected lines' text.
pub fn measure_shortlist(
    whole_document: &str,
    candidates: &[LineCandidate],
    outcome: &ShortlistOutcome,
) -> LeverMeasurement {
    let before = estimate_tokens(whole_document);
    let after: u64 = candidates
        .iter()
        .filter(|c| outcome.selected.contains(&c.line))
        .map(|c| estimate_tokens(&c.text))
        .sum();
    LeverMeasurement {
        lever: "p2_read_shortlist".to_owned(),
        tokens_before: before,
        tokens_after: after,
    }
}

// ---------------------------------------------------------------------------
// P3 — what the compactor must see
// ---------------------------------------------------------------------------

/// One compaction segment offered to the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSegment {
    pub id: String,
    pub summary: String,
    /// Pinned segments are always kept: edited files, running work, user asks.
    pub pinned: bool,
}

/// Result of a P3 decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorteOutcome {
    /// Segment ids the summarizer must see.
    pub keep: Vec<String>,
    /// Segment ids that can be dropped without summarizing.
    pub dropped: Vec<String>,
    /// True when the conservative path was taken (keep everything).
    pub fallback_full: bool,
}

/// Probability at or above which a segment is kept for the summarizer.
pub const P3_KEEP_AT_LEAST: f64 = 0.30;

/// One `noul` per segment: "must this segment be summarized verbatim?".
pub fn compaction_questions(
    segments: &[CompactionSegment],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if segments.is_empty() {
        return Err(JevError::invalid("no segments to decide about"));
    }
    if segments.len() > MAX_CHOICE_OPTIONS {
        return Err(JevError::invalid(format!(
            "{} segments over the {MAX_CHOICE_OPTIONS} question ceiling: batch them",
            segments.len()
        )));
    }
    let mut questions = BTreeMap::new();
    for segment in segments {
        questions.insert(
            format!("segment_{}", segment.id),
            Question::noul_with_criteria(
                format!(
                    "Must segments[{}] reach the summarizer to preserve the active request, user constraints, unresolved failures or work state?",
                    segment.id
                ),
                "Losing its details would lose required context or state",
                "It is redundant or irrelevant; dropping it entirely loses no required information",
            ),
        );
    }
    Ok(questions)
}

/// Composes the recorte, always preserving pinned segments.
pub fn compose_recorte(
    answers: Option<&JevAnswerSet>,
    segments: &[CompactionSegment],
) -> RecorteOutcome {
    let everything = || RecorteOutcome {
        keep: segments.iter().map(|s| s.id.clone()).collect(),
        dropped: Vec::new(),
        fallback_full: true,
    };
    let Some(answers) = answers else {
        return everything();
    };
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    for segment in segments {
        if segment.pinned {
            keep.push(segment.id.clone());
            continue;
        }
        match answers.noul(&format!("segment_{}", segment.id)) {
            Some(probability) if probability >= P3_KEEP_AT_LEAST => keep.push(segment.id.clone()),
            Some(_) => dropped.push(segment.id.clone()),
            None => return everything(),
        }
    }
    RecorteOutcome {
        keep,
        dropped,
        fallback_full: false,
    }
}

/// One conversation item as the recorte sees it: the text a summary would have
/// to carry, whether it opens a real user turn, and whether that turn touched
/// files (both pinned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInput {
    pub text: String,
    pub starts_user_turn: bool,
    pub edits_files: bool,
}

/// Fewer segments than this and the recorte costs more than it saves.
pub const P3_MIN_SEGMENTS: usize = 4;
/// Segments over this are refused: one `noul` each would bloat the state.
pub const P3_MAX_SEGMENTS: usize = 40;
/// Characters of each segment preview used as the question text.
pub const P3_PREVIEW_CHARS: usize = 120;

/// Groups the conversation into segments: a new segment starts at every real
/// user turn. The prefix, the newest segment, and every segment that touched
/// files are pinned, so the summarizer never loses state the harness knows it
/// needs.
pub fn segment_conversation(items: &[SegmentInput]) -> Vec<CompactionSegment> {
    let mut grouped: Vec<Vec<&SegmentInput>> = Vec::new();
    for item in items {
        if item.starts_user_turn || grouped.is_empty() {
            grouped.push(Vec::new());
        }
        if let Some(last) = grouped.last_mut() {
            last.push(item);
        }
    }
    let last_index = grouped.len().saturating_sub(1);
    grouped
        .iter()
        .enumerate()
        .map(|(index, segment)| CompactionSegment {
            id: format!("seg-{index}"),
            summary: preview_of(segment),
            pinned: index == 0
                || index == last_index
                || segment.iter().any(|item| item.edits_files),
        })
        .collect()
}

/// A whitespace-normalized, bounded preview of one segment.
fn preview_of(segment: &[&SegmentInput]) -> String {
    segment
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(P3_PREVIEW_CHARS)
        .collect()
}

/// Token measurement for P3: all segment summaries versus the kept ones.
pub fn measure_recorte(
    segments: &[CompactionSegment],
    outcome: &RecorteOutcome,
) -> LeverMeasurement {
    let before: u64 = segments.iter().map(|s| estimate_tokens(&s.summary)).sum();
    let after: u64 = segments
        .iter()
        .filter(|s| outcome.keep.contains(&s.id))
        .map(|s| estimate_tokens(&s.summary))
        .sum();
    LeverMeasurement {
        lever: "p3_compaction_recorte".to_owned(),
        tokens_before: before,
        tokens_after: after,
    }
}

// ---------------------------------------------------------------------------
// P6 — which announced skill matters
// ---------------------------------------------------------------------------

/// One announced skill the harness can suggest (name + one-line description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidate {
    pub name: String,
    pub description: String,
}

/// Result of a P6 decision.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillSuggestion {
    /// The chosen skill, or `None` when nothing (or nothing confidently) fits.
    pub skill: Option<String>,
    /// Probability mass the model put on "no skill applies".
    pub none_probability: Option<f64>,
    pub confidence: Option<f64>,
}

/// Top-label probability needed to name a skill; below it the suggestion is
/// dropped entirely (item 111: an uncertain pick falls back to the generic path).
pub const P6_MIN_CONFIDENCE: f64 = 0.60;
/// Label used for "nothing in this turn needs a skill".
pub const P6_NONE_LABEL: &str = "none";

/// One `choice` over the announced skills plus the explicit `none` option.
pub fn skill_questions(
    skills: &[SkillCandidate],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    if skills.is_empty() {
        return Err(JevError::invalid("no announced skills to rank"));
    }
    if skills.len() + 1 > MAX_CHOICE_OPTIONS {
        return Err(JevError::invalid(format!(
            "{} skills over the ceiling for one choice question",
            skills.len()
        )));
    }
    let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
    for skill in skills {
        criteria.insert(skill.name.clone(), Json::String(skill.description.clone()));
    }
    criteria.insert(
        P6_NONE_LABEL.to_owned(),
        Json::String("Nothing in this turn needs a skill".to_owned()),
    );
    let mut questions = BTreeMap::new();
    questions.insert(
        "best_skill".to_owned(),
        Question::choice(
            "Which single announced skill does this turn need, if any? Choose `none` when nothing applies.",
            criteria,
        )?,
    );
    Ok(questions)
}

/// Composes the suggestion, dropping anything uncertain (never a second call).
pub fn compose_skill_suggestion(answers: &JevAnswerSet) -> SkillSuggestion {
    let confidence = answers.confidence("best_skill");
    let none_probability = answers.probability("best_skill", P6_NONE_LABEL);
    let choice = answers.choice("best_skill").map(str::to_owned);
    let Some(choice) = choice else {
        return SkillSuggestion {
            skill: None,
            none_probability,
            confidence,
        };
    };
    if choice == P6_NONE_LABEL {
        return SkillSuggestion {
            skill: None,
            none_probability,
            confidence,
        };
    }
    if confidence.unwrap_or(0.0) < P6_MIN_CONFIDENCE {
        return SkillSuggestion {
            skill: None,
            none_probability,
            confidence,
        };
    }
    SkillSuggestion {
        skill: Some(choice),
        none_probability,
        confidence,
    }
}

/// Token measurement for P6: the whole announcement versus one suggestion line.
pub fn measure_skill_suggestion(
    announcement: &str,
    suggestion: &SkillSuggestion,
) -> LeverMeasurement {
    let before = estimate_tokens(announcement);
    let after = match &suggestion.skill {
        Some(name) => estimate_tokens(name) + 4,
        // "nothing applies" still ships one sentence (item 101).
        None => estimate_tokens("no skill applies to this turn"),
    };
    LeverMeasurement {
        lever: "p6_skill_suggestion".to_owned(),
        tokens_before: before,
        tokens_after: after,
    }
}

// ---------------------------------------------------------------------------
// Sink helpers (shared shape with the permission pack)
// ---------------------------------------------------------------------------

/// Records a ladder decision through the sink.
pub fn emit_ladder(
    sink: &dyn DecisionSink,
    lever: &str,
    questions: &[&str],
    decision: &JevDecision,
    answers: &JevAnswerSet,
) {
    let record = record_for(lever, questions, decision, None, answers);
    sink.record(&record);
}

/// Turns a P1/P2/P3 "no answers" case into an explicit escalation decision, so
/// every lever reports through telemetry even when it defers to the old path.
pub fn escalate_for(lever: &str, reason: impl Into<String>) -> JevDecision {
    JevDecision::Escalate {
        reason: format!("{lever}: {}", reason.into()),
    }
}

/// Convenience: the `Answer` type is part of the public surface of this module
/// (tests and the measurement harness build synthetic answer sets).
pub type LadderAnswer = Answer;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::types::Usage;

    fn answer_set(entries: Vec<(&str, Answer)>) -> JevAnswerSet {
        JevAnswerSet {
            model: "jev-1.13.0".to_owned(),
            answers: entries
                .into_iter()
                .map(|(id, answer)| (id.to_owned(), answer))
                .collect(),
            usage: Usage {
                input_tokens: Some(500),
                output_tokens: Some(40),
            },
            request_id: Some("req-ladder".to_owned()),
            latency_ms: 90,
        }
    }

    fn noul(value: f64) -> Answer {
        Answer::Noul { noul: value }
    }

    fn families() -> Vec<ToolFamily> {
        vec![
            ToolFamily::new("read", "read_file, grep, list_dir", &["read_file", "grep"]),
            ToolFamily::new("edit", "search_replace, write", &["search_replace"]),
            ToolFamily::new("execute", "bash, kill_task", &["bash"]),
            ToolFamily::new("web", "web_search, web_fetch", &["web_search", "web_fetch"]),
            ToolFamily::new("delegate", "task, task_output", &["task", "task_output"]),
        ]
    }

    #[test]
    fn p1_keeps_core_and_drops_irrelevant_families() {
        let families = families();
        let answers = answer_set(vec![
            ("family_web", noul(0.05)),
            ("family_delegate", noul(0.02)),
            ("family_edit", noul(0.8)),
        ]);
        let selection = select_tool_families(Some(&answers), &families, &["read", "execute"]);
        assert!(!selection.kept_everything);
        assert!(selection.families.contains(&"read".to_owned()));
        assert!(selection.families.contains(&"execute".to_owned()));
        assert!(selection.families.contains(&"edit".to_owned()));
        assert!(!selection.families.contains(&"web".to_owned()));
        assert!(!selection.tools.contains(&"web_search".to_owned()));
        assert!(selection.tools.contains(&"bash".to_owned()));
    }

    #[test]
    fn p1_keeps_everything_when_an_answer_is_missing() {
        let families = families();
        let answers = answer_set(vec![("family_web", noul(0.0))]);
        let selection = select_tool_families(Some(&answers), &families, &["read"]);
        assert!(selection.kept_everything);
        assert_eq!(selection.families.len(), families.len());
        let none = select_tool_families(None, &families, &["read"]);
        assert!(none.kept_everything);
    }

    #[test]
    fn p1_question_ids_and_measurement() {
        let families = families();
        let questions = tool_family_questions(&families).expect("questions build");
        assert_eq!(questions.len(), families.len());
        assert!(questions.contains_key("family_web"));

        let mut schemas = BTreeMap::new();
        for family in &families {
            for tool in &family.tools {
                schemas.insert(tool.clone(), format!("{tool} schema {}", "x".repeat(400)));
            }
        }
        let answers = answer_set(vec![
            ("family_web", noul(0.01)),
            ("family_delegate", noul(0.01)),
            ("family_edit", noul(0.9)),
        ]);
        let selection = select_tool_families(Some(&answers), &families, &["read", "execute"]);
        let measurement = measure_tool_selection(&schemas, &selection);
        println!(
            "MEASURE p1: families kept {}/{}, tools kept {}/{}, tokens {} -> {} (saved {})",
            selection.families.len(),
            families.len(),
            selection.tools.len(),
            schemas.len(),
            measurement.tokens_before,
            measurement.tokens_after,
            measurement.saved()
        );
        assert!(measurement.saved() > 0, "pruning must save tokens");
        assert!(measurement.tokens_after < measurement.tokens_before);
    }

    #[test]
    fn p2_ranks_candidates_and_falls_back_when_nothing_answers() {
        let candidates = vec![
            LineCandidate {
                line: 10,
                text: "fn main() {}".to_owned(),
            },
            LineCandidate {
                line: 42,
                text: "let timeout = 30;".to_owned(),
            },
            LineCandidate {
                line: 77,
                text: "unrelated".to_owned(),
            },
        ];
        let (state, questions) =
            shortlist_request(&candidates, "find the timeout").expect("questions build");
        assert_eq!(state["request"], "find the timeout");
        assert_eq!(state["candidates"]["42"], "let timeout = 30;");
        assert!(shortlist_request(&candidates, "").is_err());
        assert!(questions.contains_key("keep_42"));
        let ranked = answer_set(vec![
            ("keep_10", noul(0.01)),
            ("keep_42", noul(0.9)),
            ("keep_77", noul(0.01)),
            ("answer_exists", noul(0.9)),
            ("narrow_safe", noul(0.99)),
        ]);
        let outcome = compose_shortlist(&ranked, &candidates);
        assert!(!outcome.no_answer);
        assert_eq!(outcome.selected, vec![10, 42, 77], "document edges survive");
        let missing = answer_set(vec![
            ("answer_exists", noul(0.9)),
            ("narrow_safe", noul(0.99)),
        ]);
        assert!(compose_shortlist(&missing, &candidates).no_answer);
        let broad = answer_set(vec![
            ("answer_exists", noul(0.9)),
            ("narrow_safe", noul(0.1)),
        ]);
        assert!(compose_shortlist(&broad, &candidates).no_answer);

        let whole = "a\n".repeat(2_000);
        let measurement = measure_shortlist(
            &whole,
            &candidates,
            &compose_shortlist(&ranked, &candidates),
        );
        println!(
            "MEASURE p2: document tokens {} -> selected {} lines {} (saved {})",
            measurement.tokens_before,
            measurement.tokens_after,
            outcome.selected.len(),
            measurement.saved()
        );
        assert!(measurement.saved() > 0);
    }

    #[test]
    fn p2_preserves_fences_and_finds_blocks_after_line_200() {
        let text = format!(
            "{}\n\n```rust\na\n\nb\n```\n\nanswer at end",
            "intro\n".repeat(220)
        );
        let blocks = paragraph_candidates(&text);
        assert_eq!(blocks.len(), 3);
        assert!(blocks[1].text.contains("a\n\nb"));
        assert!(blocks[2].line > 200);
        assert_eq!(
            paragraph_candidates("````md\n```\n\ninside\n```\n````\n\nafter").len(),
            2
        );
        let candidates = paragraph_candidates("intro\n\nnoise\n\nanswer\n\nend");
        let answers = answer_set(vec![
            ("answer_exists", noul(0.99)),
            ("narrow_safe", noul(0.99)),
            ("keep_1", noul(0.01)),
            ("keep_3", noul(0.01)),
            ("keep_5", noul(0.99)),
            ("keep_7", noul(0.01)),
        ]);
        assert_eq!(
            compose_shortlist(&answers, &candidates).selected,
            vec![1, 5, 7]
        );
    }

    #[test]
    fn p2_rejects_more_candidates_than_one_choice_allows() {
        let many: Vec<LineCandidate> = (0..(MAX_CHOICE_OPTIONS + 1))
            .map(|i| LineCandidate {
                line: i,
                text: format!("line {i}"),
            })
            .collect();
        assert!(shortlist_request(&many, "find the timeout").is_err());
    }

    #[test]
    fn p3_keeps_pinned_segments_and_falls_back_on_missing_answers() {
        let segments = vec![
            CompactionSegment {
                id: "a".to_owned(),
                summary: "user asked for X".to_owned(),
                pinned: true,
            },
            CompactionSegment {
                id: "b".to_owned(),
                summary: "long build log".to_owned(),
                pinned: false,
            },
            CompactionSegment {
                id: "c".to_owned(),
                summary: "small talk".to_owned(),
                pinned: false,
            },
        ];
        let questions = compaction_questions(&segments).expect("questions build");
        assert_eq!(questions.len(), 3);

        let answers = answer_set(vec![("segment_b", noul(0.8)), ("segment_c", noul(0.05))]);
        let outcome = compose_recorte(Some(&answers), &segments);
        assert!(!outcome.fallback_full);
        assert_eq!(outcome.keep, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(outcome.dropped, vec!["c".to_owned()]);
        let measurement = measure_recorte(&segments, &outcome);
        println!(
            "MEASURE p3: segments kept {}/{}, tokens {} -> {} (saved {})",
            outcome.keep.len(),
            segments.len(),
            measurement.tokens_before,
            measurement.tokens_after,
            measurement.saved()
        );
        assert!(measurement.saved() > 0);

        let incomplete = answer_set(vec![("segment_b", noul(0.8))]);
        let fallback = compose_recorte(Some(&incomplete), &segments);
        assert!(fallback.fallback_full);
        assert_eq!(fallback.keep.len(), 3);
        assert!(compose_recorte(None, &segments).fallback_full);
    }

    #[test]
    fn p3_segments_start_at_user_turns_and_pin_the_edges() {
        let item = |text: &str, starts_user_turn: bool, edits_files: bool| SegmentInput {
            text: text.to_owned(),
            starts_user_turn,
            edits_files,
        };
        let segments = segment_conversation(&[
            item("system prefix", false, false),
            item("first request", true, false),
            item("read a file", false, false),
            item("second request", true, false),
            item("wrote a file", false, true),
            item("third request", true, false),
        ]);
        assert_eq!(segments.len(), 4, "prefix plus one segment per user turn");
        assert!(segments[0].pinned, "the prefix is always kept");
        assert!(segments[3].pinned, "the newest segment is always kept");
        assert!(
            segments[2].pinned,
            "a segment that touched files is always kept"
        );
        assert!(!segments[1].pinned);
        assert_eq!(segments[1].summary, "first request read a file");

        // The preview is bounded, so one huge segment cannot bloat the state.
        let huge = segment_conversation(&[SegmentInput {
            text: "x".repeat(P3_PREVIEW_CHARS * 4),
            starts_user_turn: true,
            edits_files: false,
        }]);
        assert_eq!(huge[0].summary.chars().count(), P3_PREVIEW_CHARS);

        // A conversation opening on a user turn still gets a prefix segment.
        let no_prefix = segment_conversation(&[item("only request", true, false)]);
        assert_eq!(no_prefix.len(), 1);
        assert!(no_prefix[0].pinned);
    }

    #[test]
    fn p6_suggests_only_confidently_and_measures_the_line() {
        let skills = vec![
            SkillCandidate {
                name: "graphify".to_owned(),
                description: "knowledge graph from any input".to_owned(),
            },
            SkillCandidate {
                name: "pdf".to_owned(),
                description: "read and create PDFs".to_owned(),
            },
        ];
        let questions = skill_questions(&skills).expect("questions build");
        assert!(questions.contains_key("best_skill"));

        let confident = answer_set(vec![(
            "best_skill",
            Answer::Choice {
                choice: "pdf".to_owned(),
                probabilities: [
                    ("pdf".to_owned(), 0.8),
                    ("graphify".to_owned(), 0.1),
                    (P6_NONE_LABEL.to_owned(), 0.1),
                ]
                .into_iter()
                .collect(),
                confidence: Some(0.8),
            },
        )]);
        let suggestion = compose_skill_suggestion(&confident);
        assert_eq!(suggestion.skill.as_deref(), Some("pdf"));

        let unsure = answer_set(vec![(
            "best_skill",
            Answer::Choice {
                choice: "pdf".to_owned(),
                probabilities: [("pdf".to_owned(), 0.5), (P6_NONE_LABEL.to_owned(), 0.5)]
                    .into_iter()
                    .collect(),
                confidence: Some(0.5),
            },
        )]);
        assert_eq!(compose_skill_suggestion(&unsure).skill, None);

        let announcement = "skill: graphify — …\n".repeat(50);
        let measurement = measure_skill_suggestion(&announcement, &suggestion);
        println!(
            "MEASURE p6: announcement tokens {} -> suggestion line {} (saved {})",
            measurement.tokens_before,
            measurement.tokens_after,
            measurement.saved()
        );
        assert!(measurement.saved() > 0);
    }

    #[test]
    fn measurement_ledger_totals_every_lever() {
        let mut ledger = MeasurementLedger::new();
        ledger.push(LeverMeasurement {
            lever: "p1_tool_family".to_owned(),
            tokens_before: 6_000,
            tokens_after: 2_000,
        });
        ledger.push(LeverMeasurement {
            lever: "p2_read_shortlist".to_owned(),
            tokens_before: 9_000,
            tokens_after: 1_000,
        });
        assert_eq!(ledger.total_saved(), 12_000);
        assert_eq!(ledger.entries().len(), 2);
        assert!(P4_DOC.contains("money lever"));
        println!("MEASURE p4: 0 tokens by design — {P4_DOC}");
        println!(
            "MEASURE p5: no token delta of its own; its saving is the round trip a failed call would cost (100-200k late in a session, plan §1.3.1)"
        );
    }
}
