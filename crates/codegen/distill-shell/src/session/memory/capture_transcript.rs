// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use distill_sampling_types::{ContentPart, ConversationItem};

/// ~130k tokens of JSON; leaves room for the extractor's output in the context window.
pub(crate) const BASE_MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
pub(crate) const BASE_MAX_TRANSCRIPT_ITEMS: usize = 512;
const MIN_MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MIN_MAX_TRANSCRIPT_ITEMS: usize = 64;

const TOOL_RESULT_TIERS: [usize; 3] = [4 * 1024, 1024, 256];
const TOOL_ARGS_TIERS: [usize; 3] = [2 * 1024, 512, 256];
const KEEP_EDGE_START: usize = 8;
const USER_TEXT_FLOOR_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptBudget {
    pub max_bytes: usize,
    pub max_items: usize,
}

impl TranscriptBudget {
    pub(crate) fn for_attempt(attempt: u32) -> Self {
        let shift = attempt.saturating_sub(1).min(8);
        Self {
            max_bytes: (BASE_MAX_TRANSCRIPT_BYTES >> shift).max(MIN_MAX_TRANSCRIPT_BYTES),
            max_items: (BASE_MAX_TRANSCRIPT_ITEMS >> shift).max(MIN_MAX_TRANSCRIPT_ITEMS),
        }
    }

    fn fits(self, json: &str, items: usize) -> bool {
        json.len() <= self.max_bytes && items <= self.max_items
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CondensationStats {
    pub original_bytes: usize,
    pub original_items: usize,
    pub final_bytes: usize,
    pub final_items: usize,
    pub reasoning_dropped: usize,
    pub images_dropped: usize,
    pub tool_results_trimmed: usize,
    pub tool_args_trimmed: usize,
    pub steps_omitted: usize,
    pub user_text_trimmed: usize,
    pub over_budget: bool,
}

impl CondensationStats {
    pub(crate) fn is_condensed(&self) -> bool {
        self.reasoning_dropped > 0
            || self.images_dropped > 0
            || self.tool_results_trimmed > 0
            || self.tool_args_trimmed > 0
            || self.steps_omitted > 0
            || self.user_text_trimmed > 0
    }

    pub(crate) fn prompt_note(&self) -> Option<String> {
        if !self.is_condensed() {
            return None;
        }
        let mut parts = Vec::new();
        if self.reasoning_dropped > 0 {
            parts.push("model reasoning omitted".to_owned());
        }
        if self.images_dropped > 0 {
            parts.push("images omitted".to_owned());
        }
        if self.tool_results_trimmed > 0 {
            parts.push("tool outputs truncated".to_owned());
        }
        if self.tool_args_trimmed > 0 {
            parts.push("tool arguments truncated".to_owned());
        }
        if self.steps_omitted > 0 {
            parts.push(format!("{} intermediate steps omitted", self.steps_omitted));
        }
        if self.user_text_trimmed > 0 {
            parts.push("user text truncated".to_owned());
        }
        Some(format!(
            "The transcript was condensed to fit the extraction budget ({}). \
             Text between \"[...\" and \"...]\" markers was removed, not written by a participant.",
            parts.join(", ")
        ))
    }
}

#[derive(Debug)]
pub(crate) struct CondensedTranscript {
    pub json: String,
    pub stats: CondensationStats,
}

/// Stages remove what the extractor does not need before touching what it does:
/// reasoning and images, then tool bodies, then the middle of the tool loop,
/// then the user's own words. Never fails on size.
pub(crate) fn condense_turn_transcript(
    mut items: Vec<ConversationItem>,
    budget: TranscriptBudget,
) -> Result<CondensedTranscript, String> {
    let mut stats = CondensationStats {
        original_items: items.len(),
        ..CondensationStats::default()
    };
    let mut json = serialize(&items)?;
    stats.original_bytes = json.len();
    if budget.fits(&json, items.len()) {
        return Ok(finish(json, items.len(), stats));
    }

    let before = items.len();
    items.retain(|item| {
        !matches!(
            item,
            ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_)
        )
    });
    stats.reasoning_dropped = before - items.len();
    for item in &mut items {
        stats.images_dropped += strip_images(item);
    }
    json = serialize(&items)?;
    if budget.fits(&json, items.len()) {
        return Ok(finish(json, items.len(), stats));
    }

    for (result_cap, args_cap) in TOOL_RESULT_TIERS.into_iter().zip(TOOL_ARGS_TIERS) {
        for item in &mut items {
            match item {
                ConversationItem::ToolResult(result) => {
                    if let Some(trimmed) = trim_middle(&result.content, result_cap) {
                        result.content = Arc::from(trimmed);
                        stats.tool_results_trimmed += 1;
                    }
                }
                ConversationItem::Assistant(assistant) => {
                    for call in &mut assistant.tool_calls {
                        if let Some(trimmed) = trim_middle(&call.arguments, args_cap) {
                            call.arguments = Arc::from(trimmed);
                            stats.tool_args_trimmed += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        json = serialize(&items)?;
        if budget.fits(&json, items.len()) {
            return Ok(finish(json, items.len(), stats));
        }
    }

    let mut keep_edge = KEEP_EDGE_START;
    loop {
        let (reduced, omitted) = omit_middle_steps(&items, keep_edge);
        let reduced_json = serialize(&reduced)?;
        if budget.fits(&reduced_json, reduced.len()) || keep_edge == 1 {
            let final_items = reduced.len();
            items = reduced;
            json = reduced_json;
            stats.steps_omitted = omitted;
            if budget.fits(&json, final_items) {
                return Ok(finish(json, final_items, stats));
            }
            break;
        }
        keep_edge /= 2;
    }

    let user_count = items
        .iter()
        .filter(|item| matches!(item, ConversationItem::User(_)))
        .count()
        .max(1);
    let other_bytes: usize = items
        .iter()
        .filter(|item| !matches!(item, ConversationItem::User(_)))
        .map(|item| serde_json::to_string(item).map(|s| s.len()).unwrap_or(0))
        .sum();
    let mut share = budget
        .max_bytes
        .saturating_sub(other_bytes)
        .checked_div(user_count)
        .unwrap_or(0)
        .max(USER_TEXT_FLOOR_BYTES);
    loop {
        let mut trimmed_any = false;
        for item in &mut items {
            if let ConversationItem::User(user) = item {
                for part in &mut user.content {
                    if let ContentPart::Text { text } = part
                        && let Some(trimmed) = trim_middle(text, share)
                    {
                        *text = Arc::from(trimmed);
                        trimmed_any = true;
                    }
                }
            }
        }
        if trimmed_any {
            stats.user_text_trimmed += 1;
        }
        json = serialize(&items)?;
        if budget.fits(&json, items.len()) || share <= USER_TEXT_FLOOR_BYTES {
            break;
        }
        // JSON escaping and the envelope cost bytes the share did not count.
        share = (share * 9 / 10).max(USER_TEXT_FLOOR_BYTES);
    }
    stats.over_budget = !budget.fits(&json, items.len());
    Ok(finish(json, items.len(), stats))
}

fn finish(json: String, items: usize, mut stats: CondensationStats) -> CondensedTranscript {
    stats.final_bytes = json.len();
    stats.final_items = items;
    CondensedTranscript { json, stats }
}

fn serialize(items: &[ConversationItem]) -> Result<String, String> {
    serde_json::to_string(items)
        .map_err(|error| format!("durable transcript encoding failed: {error}"))
}

fn strip_images(item: &mut ConversationItem) -> usize {
    let mut dropped = 0;
    let mut replace = |parts: &mut Vec<ContentPart>| {
        for part in parts.iter_mut() {
            if matches!(part, ContentPart::Image { .. }) {
                *part = ContentPart::Text {
                    text: Arc::from("[... image omitted ...]"),
                };
                dropped += 1;
            }
        }
    };
    match item {
        ConversationItem::User(user) => replace(&mut user.content),
        ConversationItem::ToolResult(result) => replace(&mut result.images),
        _ => {}
    }
    dropped
}

fn trim_middle(text: &str, max_bytes: usize) -> Option<String> {
    if text.len() <= max_bytes {
        return None;
    }
    let head_len = max_bytes * 2 / 3;
    let tail_len = max_bytes.saturating_sub(head_len);
    let head_end = floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_len)).max(head_end);
    let head = text.get(..head_end).unwrap_or_default();
    let tail = text.get(tail_start..).unwrap_or_default();
    let omitted = text.len().saturating_sub(head.len() + tail.len());
    Some(format!("{head}\n[... {omitted} bytes omitted ...]\n{tail}"))
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn omit_middle_steps(
    items: &[ConversationItem],
    keep_edge: usize,
) -> (Vec<ConversationItem>, usize) {
    let step_positions: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !matches!(item, ConversationItem::User(_)))
        .map(|(index, _)| index)
        .collect();
    if step_positions.len() <= keep_edge * 2 {
        return (items.to_vec(), 0);
    }
    let (Some(&first_dropped), Some(&last_dropped)) = (
        step_positions.get(keep_edge),
        step_positions.get(step_positions.len().saturating_sub(keep_edge + 1)),
    ) else {
        return (items.to_vec(), 0);
    };
    let mut reduced = Vec::with_capacity(items.len());
    let mut omitted = 0;
    let mut marker_placed = false;
    for (index, item) in items.iter().enumerate() {
        let is_step = !matches!(item, ConversationItem::User(_));
        if is_step && (first_dropped..=last_dropped).contains(&index) {
            omitted += 1;
            if !marker_placed {
                reduced.push(ConversationItem::assistant(
                    "[... intermediate steps omitted ...]",
                ));
                marker_placed = true;
            }
            continue;
        }
        reduced.push(item.clone());
    }
    if let Some(ConversationItem::Assistant(marker)) = reduced.iter_mut().find(|item| {
        matches!(item, ConversationItem::Assistant(a) if a.content.as_ref() == "[... intermediate steps omitted ...]")
    }) {
        marker.content = Arc::from(format!("[... {omitted} intermediate steps omitted ...]"));
    }
    (reduced, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning(text: String) -> ConversationItem {
        ConversationItem::Reasoning(distill_sampling_types::synthesized_reasoning_item(text))
    }

    fn tool_loop(steps: usize, result_bytes: usize) -> Vec<ConversationItem> {
        let mut items = vec![ConversationItem::user("please refactor the workflow modal")];
        for step in 0..steps {
            items.push(reasoning(format!("thinking about step {step}")));
            let mut assistant = ConversationItem::assistant(format!("step {step}"));
            if let ConversationItem::Assistant(a) = &mut assistant {
                a.tool_calls.push(distill_sampling_types::ToolCall {
                    id: Arc::from(format!("call-{step}")),
                    name: "read_file".to_owned(),
                    arguments: Arc::from(format!("{{\"path\":\"src/file{step}.rs\"}}")),
                });
            }
            items.push(assistant);
            items.push(ConversationItem::tool_result(
                format!("call-{step}"),
                "x".repeat(result_bytes),
            ));
        }
        items.push(ConversationItem::assistant(
            "Done: the modal now confirms before deleting.",
        ));
        items
    }

    fn contains_all_users(json: &str, items: &[ConversationItem]) -> bool {
        items.iter().all(|item| match item {
            ConversationItem::User(user) => user.content.iter().all(|part| match part {
                ContentPart::Text { text } => json.contains(text.as_ref()),
                ContentPart::Image { .. } => true,
            }),
            _ => true,
        })
    }

    #[test]
    fn under_budget_turn_is_byte_identical() {
        let items = tool_loop(3, 100);
        let plain = serde_json::to_string(&items).unwrap();
        let condensed = condense_turn_transcript(items, TranscriptBudget::for_attempt(1)).unwrap();
        assert_eq!(condensed.json, plain);
        assert!(!condensed.stats.is_condensed());
        assert!(condensed.stats.prompt_note().is_none());
    }

    #[test]
    fn tool_results_trim_head_and_tail_with_marker() {
        let items = tool_loop(60, 20 * 1024);
        let budget = TranscriptBudget::for_attempt(1);
        let condensed = condense_turn_transcript(items.clone(), budget).unwrap();
        assert!(condensed.json.len() <= budget.max_bytes);
        assert!(condensed.stats.tool_results_trimmed > 0);
        assert!(condensed.json.contains("bytes omitted"));
        assert!(contains_all_users(&condensed.json, &items));
        assert!(
            condensed
                .json
                .contains("the modal now confirms before deleting")
        );
        assert_eq!(condensed.stats.steps_omitted, 0);
    }

    #[test]
    fn s4_shaped_turn_fits_first_attempt_budget() {
        let mut items = vec![ConversationItem::user("u".repeat(36 * 1024))];
        for step in 0..126 {
            if step % 3 == 0 {
                items.push(reasoning("r".repeat(4700)));
            }
            items.push(ConversationItem::assistant(format!("step {step}")));
            let size = if step % 20 == 0 { 14 * 1024 } else { 1800 };
            items.push(ConversationItem::tool_result(
                format!("c{step}"),
                "t".repeat(size),
            ));
        }
        items.push(ConversationItem::assistant("final summary"));
        let plain = serde_json::to_string(&items).unwrap().len();
        assert!(
            plain > BASE_MAX_TRANSCRIPT_BYTES,
            "fixture must overflow: {plain}"
        );
        let condensed = condense_turn_transcript(items, TranscriptBudget::for_attempt(1)).unwrap();
        assert!(condensed.json.len() <= BASE_MAX_TRANSCRIPT_BYTES);
        assert!(condensed.json.contains("final summary"));
        assert!(condensed.stats.reasoning_dropped > 0);
        assert_eq!(condensed.stats.steps_omitted, 0);
        assert!(!condensed.stats.over_budget);
    }

    #[test]
    fn thousand_step_turn_keeps_user_and_edges() {
        let items = tool_loop(1000, 3000);
        let budget = TranscriptBudget::for_attempt(1);
        let condensed = condense_turn_transcript(items.clone(), budget).unwrap();
        assert!(condensed.json.len() <= budget.max_bytes);
        assert!(condensed.stats.final_items <= budget.max_items);
        assert!(condensed.stats.steps_omitted > 0);
        assert!(condensed.json.contains("intermediate steps omitted"));
        assert!(
            condensed
                .json
                .contains("please refactor the workflow modal")
        );
        assert!(
            condensed
                .json
                .contains("the modal now confirms before deleting")
        );
        assert!(condensed.json.contains("step 0\""));
        assert!(!condensed.stats.over_budget);
    }

    #[test]
    fn oversized_user_text_is_trimmed_last_and_never_fails() {
        let items = vec![
            ConversationItem::user("p".repeat(900 * 1024)),
            ConversationItem::assistant("short answer"),
        ];
        let budget = TranscriptBudget::for_attempt(1);
        let condensed = condense_turn_transcript(items, budget).unwrap();
        assert!(condensed.stats.user_text_trimmed >= 1);
        assert!(condensed.json.len() <= budget.max_bytes);
        assert!(condensed.json.contains("short answer"));
        assert!(!condensed.stats.over_budget);
    }

    #[test]
    fn trim_middle_respects_char_boundaries() {
        let text = "é".repeat(1000);
        let trimmed = trim_middle(&text, 100).unwrap();
        assert!(trimmed.contains("bytes omitted"));
        assert!(trimmed.starts_with('é') && trimmed.ends_with('é'));
        assert!(trim_middle("short", 100).is_none());
    }
}
