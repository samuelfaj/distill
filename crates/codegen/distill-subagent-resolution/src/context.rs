// Modified for Distill by Samuel Fajreldines, 2026.
//! Fork-context normalization: summarizes parent conversation for child sessions.
//!
//! Extracted from `distill-shell/src/agent/subagent/` `normalize_forked_context()`.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Write;

use distill_sampling_types::conversation::ConversationItem;

/// Maximum number of complete turns to render verbatim in the background context.
/// Turns beyond this threshold (counting from the end) are summarized as metadata (message counts and tools used).
const MAX_VERBATIM_TURNS: usize = 3;

/// XML tags whose content is stripped from user messages during fork context normalization. The child session's system
/// prompt builder re-injects these blocks, so keeping them in the background context duplicates them. See also:
/// `distill-chat-state::compaction_utils::strip_system_tags`, which strips a related (but different) tag set for compaction.
const FORK_NOISE_TAGS: &[&str] = &[
    "system-reminder",
    "system_reminder", // Cursor wire format uses underscore
    "user_info",
    "git_status",
    "project_layout",
    "attached_files", // File context attached by an alternate agent; the child reads files itself
];

/// The System item is kept as-is (replaced later by `spawn_session_actor`). The task prompt is NOT included here: it
/// arrives via the normal Prompt command and becomes the last user message (position [2]). `inherited_prefix_len` is the
/// number of items the child should treat as pre-existing context (typically 2 for `[System, BackgroundContext]`).
pub fn normalize_forked_context(items: Vec<ConversationItem>) -> (Vec<ConversationItem>, usize) {
    // Extract the system prompt (position 0), kept as a placeholder for spawn_session_actor
    let system = items
        .first()
        .filter(|i| matches!(i, ConversationItem::System(_)))
        .cloned()
        .unwrap_or_else(|| ConversationItem::system(String::new()));

    // Collect non-system items as the parent context to render.
    let parent_items: Vec<&ConversationItem> = items
        .iter()
        .skip(1) // skip System
        .filter(|i| !matches!(i, ConversationItem::System(_)))
        .collect();

    if parent_items.is_empty() {
        return (vec![system], 1);
    }

    // Count complete turns (a User message, then an Assistant message, then any ToolResults)
    let turns = count_complete_turns(&parent_items);

    let mut background = String::from("<background_context>\n");
    background.push_str(
        "The following is the parent session's conversation history. \
         Use it as background context to inform your work.\n\n",
    );

    if turns.len() <= MAX_VERBATIM_TURNS {
        // All turns fit, so render them verbatim
        for item in &parent_items {
            render_item_to_background(&mut background, item);
        }
    } else {
        // Summarize early turns, keep last MAX_VERBATIM_TURNS verbatim.
        let Some(&early_end) = turns
            .len()
            .checked_sub(MAX_VERBATIM_TURNS)
            .and_then(|i| turns.get(i))
        else {
            for item in parent_items {
                render_item_to_background(&mut background, item);
            }
            background.push_str("</background_context>");
            let conversation = vec![system, ConversationItem::user(&background)];
            return (conversation, 2);
        };
        background.push_str("=== Earlier context (summarized) ===\n");
        if let Some(early) = parent_items.get(..early_end) {
            render_summary(&mut background, early);
        }
        background.push_str("\n=== Recent turns (verbatim) ===\n");
        if let Some(recent) = parent_items.get(early_end..) {
            for item in recent {
                render_item_to_background(&mut background, item);
            }
        }
    }
    background.push_str("</background_context>");

    let conversation = vec![system, ConversationItem::user(&background)];
    (conversation, 2)
}

/// The scan skips those, both before the Assistant and inside the ToolResult run that follows it. Otherwise long forked
/// histories would register zero turns and never summarize, blowing up token usage. NOTE: two scanners walk turn
/// boundaries while skipping `Reasoning` items, and they must move together.
fn count_complete_turns(items: &[&ConversationItem]) -> Vec<usize> {
    let mut turn_ends = Vec::new();
    let mut i = 0;
    while i < items.len() {
        // Skip until the start of a turn (a User message).
        if !items
            .get(i)
            .is_some_and(|item| matches!(item, ConversationItem::User(_)))
        {
            i += 1;
            continue;
        }
        // Consume consecutive User messages.
        while items
            .get(i)
            .is_some_and(|item| matches!(item, ConversationItem::User(_)))
        {
            i += 1;
        }
        // Skip Reasoning / BackendToolCall siblings that precede the Assistant.
        while items.get(i).is_some_and(|item| {
            matches!(
                item,
                ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_)
            )
        }) {
            i += 1;
        }
        // Expect Assistant.
        if !items
            .get(i)
            .is_some_and(|item| matches!(item, ConversationItem::Assistant(_)))
        {
            break;
        }
        i += 1; // skip past Assistant
        // Consume the post-assistant run: ToolResults plus interleaved Reasoning / BackendToolCall siblings, until the next User/Assistant
        while items.get(i).is_some_and(|item| {
            matches!(
                item,
                ConversationItem::ToolResult(_)
                    | ConversationItem::Reasoning(_)
                    | ConversationItem::BackendToolCall(_)
            )
        }) {
            i += 1;
        }
        turn_ends.push(i);
    }
    turn_ends
}

/// Strip content from user message text that is redundant in a forked child context. The child session gets its own
/// system reminders, user info, git status, and project layout via the system prompt builder. Also strips the skill body
/// that follows a `</command-args>` tag; those instructions drove the parent's skill run and mean nothing to the child.
fn strip_fork_noise(text: &str) -> String {
    if !text.contains('<') {
        let mut result = collapse_blank_lines(text);
        trim_string_in_place(&mut result);
        return result;
    }

    let mut cow: Cow<'_, str> = Cow::Borrowed(text);
    for tag in FORK_NOISE_TAGS {
        if let Cow::Owned(s) = strip_xml_block(&cow, tag) {
            cow = Cow::Owned(s);
        }
    }
    if let Cow::Owned(s) = strip_skill_instructions(&cow) {
        cow = Cow::Owned(s);
    }

    let mut result = collapse_blank_lines(&cow);
    trim_string_in_place(&mut result);
    result
}

/// Remove all occurrences of `<tag...>...</tag>` from the input string. Same-name nesting is not supported: matches the
/// first closing tag. See also: `distill-chat-state::compaction_utils::strip_system_tags`, which also leaves unclosed tags
/// untouched for a different tag set.
fn strip_xml_block<'a>(text: &'a str, tag: &str) -> Cow<'a, str> {
    let open_prefix = format!("<{tag}");
    if !text.contains(&*open_prefix) {
        return Cow::Borrowed(text);
    }
    let close_tag = format!("</{tag}>");
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(open_start) = remaining.find(&open_prefix) {
        let Some(after_name) = remaining.get(open_start + open_prefix.len()..) else {
            break;
        };
        let is_tag = after_name.starts_with(|c: char| c == '>' || c.is_ascii_whitespace());
        if !is_tag {
            let Some(keep) = remaining.get(..open_start + open_prefix.len()) else {
                break;
            };
            result.push_str(keep);
            remaining = remaining
                .get(open_start + open_prefix.len()..)
                .unwrap_or("");
            continue;
        }

        if let Some(close_rel) = remaining.get(open_start..).and_then(|r| r.find(&close_tag)) {
            if let Some(head) = remaining.get(..open_start) {
                result.push_str(head);
            }
            remaining = remaining
                .get(open_start + close_rel + close_tag.len()..)
                .unwrap_or("");
        } else {
            tracing::warn!(
                tag,
                "fork context strip: unclosed <{tag}> tag, leaving untouched"
            );
            result.push_str(remaining);
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);
    Cow::Owned(result)
}

/// Strip skill instruction content from user query blocks. Preserves the command metadata tags (`<command-name>`,
/// `<command-message>`, `<command-args>`). Removes the skill body that follows `</command-args>`. The child sees the
/// command name and args but not the skill body, which only told the parent how to run the skill.
fn strip_skill_instructions<'a>(text: &'a str) -> Cow<'a, str> {
    let marker = "</command-args>";
    let Some(marker_pos) = text.find(marker) else {
        return Cow::Borrowed(text);
    };
    let after_marker = marker_pos + marker.len();

    let end_pos = text
        .get(after_marker..)
        .and_then(|rest| rest.find("</user_query>"))
        .map(|p| after_marker + p)
        .unwrap_or(text.len());

    let mut result = String::with_capacity(text.len());
    result.push_str(text.get(..after_marker).unwrap_or(""));
    result.push_str(text.get(end_pos..).unwrap_or(""));
    Cow::Owned(result)
}

/// Collapse runs of consecutive blank lines into at most one.
fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_blank = false;
    for line in text.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
        prev_blank = is_blank;
    }
    result
}

/// Trim leading and trailing whitespace from a `String` in place.
fn trim_string_in_place(s: &mut String) {
    let end = s.trim_end().len();
    s.truncate(end);
    let start = s.len() - s.trim_start().len();
    if start > 0 {
        s.drain(..start);
    }
}

/// Render a single conversation item into the background context string.
fn render_item_to_background(out: &mut String, item: &ConversationItem) {
    match item {
        ConversationItem::User(u) => {
            let text: String = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            let text = strip_fork_noise(&text);
            if text.is_empty() {
                tracing::debug!(
                    target: "fork_context",
                    "Skipping empty user message after fork noise stripping"
                );
                return;
            }
            let _ = writeln!(out, "[User]: {text}");
        }
        ConversationItem::Assistant(a) => {
            if !a.content.is_empty() {
                let _ = writeln!(out, "[Assistant]: {}", a.content);
            }
            for tc in &a.tool_calls {
                let args_preview = if tc.arguments.len() > 100 {
                    format!("{}...", truncate_str(&tc.arguments, 100))
                } else {
                    tc.arguments.as_ref().to_owned()
                };
                let _ = writeln!(out, "[Tool Call]: {} ({})", tc.name, args_preview);
            }
        }
        ConversationItem::ToolResult(tr) => {
            let preview = if tr.content.len() > 200 {
                format!("{}...", truncate_str(&tr.content, 200))
            } else {
                tr.content.as_ref().to_owned()
            };
            let _ = writeln!(out, "[Tool Result]: {preview}");
        }
        ConversationItem::System(_) => {}
        ConversationItem::BackendToolCall(b) => {
            let _ = writeln!(out, "[Backend Tool]: {}", b.text_summary());
        }
        // Reasoning siblings do not enter the background text; they render inline with the surrounding assistant turn elsewhere, when needed
        ConversationItem::Reasoning(_) => {}
    }
}

/// Render a summary of early conversation items (files mentioned, tools used).
fn render_summary(out: &mut String, items: &[&ConversationItem]) {
    let mut tools_used = BTreeSet::new();
    let mut user_messages = 0u32;
    let mut assistant_messages = 0u32;

    for item in items {
        match item {
            ConversationItem::User(_) => user_messages += 1,
            ConversationItem::Assistant(a) => {
                assistant_messages += 1;
                for tc in &a.tool_calls {
                    tools_used.insert(tc.name.clone());
                }
            }
            _ => {}
        }
    }

    let _ = writeln!(
        out,
        "  Messages: {user_messages} user, {assistant_messages} assistant"
    );
    if !tools_used.is_empty() {
        let tools: Vec<_> = tools_used.into_iter().collect();
        let _ = writeln!(out, "  Tools used: {}", tools.join(", "));
    }
}

/// Truncate a string to at most `max_chars` Unicode characters. `char_indices` finds the byte offset of the Nth
/// character, so multi-byte UTF-8 content (emoji, CJK) never splits mid-character. Returns the full string if it has
/// `max_chars` or fewer characters.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_offset, _)) => s.get(..byte_offset).unwrap_or(s),
        None => s, // string has at most max_chars characters
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_sampling_types::conversation::{ConversationItem, ToolCall, ToolResultItem};

    fn user_item(text: &str) -> ConversationItem {
        ConversationItem::user(text)
    }

    fn assistant_item(text: &str) -> ConversationItem {
        ConversationItem::assistant(text)
    }

    fn assistant_with_tool_calls(text: &str, tool_names: &[&str]) -> ConversationItem {
        let mut item = ConversationItem::assistant(text);
        if let ConversationItem::Assistant(ref mut a) = item {
            a.tool_calls = tool_names
                .iter()
                .map(|name| ToolCall {
                    id: format!("tc-{name}").into(),
                    name: name.to_string(),
                    arguments: "{}".into(),
                })
                .collect();
        }
        item
    }

    fn tool_result(content: &str) -> ConversationItem {
        ConversationItem::ToolResult(ToolResultItem {
            tool_call_id: "tc-1".to_string(),
            content: content.into(),
            images: Vec::new(),
        })
    }

    fn system_item(text: &str) -> ConversationItem {
        ConversationItem::system(text)
    }

    fn reasoning_item(text: &str) -> ConversationItem {
        ConversationItem::Reasoning(distill_sampling_types::synthesized_reasoning_item(text))
    }

    fn extract_background_text(item: &ConversationItem) -> String {
        match item {
            ConversationItem::User(u) => u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("Expected User item"),
        }
    }

    #[test]
    fn empty_parent_items_returns_system_only() {
        let items = vec![system_item("System prompt")];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 1);
        assert_eq!(result.len(), 1);
        assert!(matches!(result.first(), Some(ConversationItem::System(_))));
    }

    #[test]
    fn single_turn_rendered_verbatim() {
        let items = vec![
            system_item("System"),
            user_item("Hello"),
            assistant_item("Hi there"),
        ];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 2);
        assert_eq!(result.len(), 2);

        // Second item should be User with background_context
        if let Some(ConversationItem::User(u)) = result.get(1) {
            let text = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(text.contains("<background_context>"));
            assert!(text.contains("[User]: Hello"));
            assert!(text.contains("[Assistant]: Hi there"));
            assert!(text.contains("</background_context>"));
            assert!(!text.contains("=== Earlier context"));
        } else {
            panic!("Expected User item");
        }
    }

    #[test]
    fn three_turns_all_verbatim() {
        let items = vec![
            system_item("System"),
            user_item("Turn 1"),
            assistant_item("Response 1"),
            user_item("Turn 2"),
            assistant_item("Response 2"),
            user_item("Turn 3"),
            assistant_item("Response 3"),
        ];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 2);

        if let Some(ConversationItem::User(u)) = result.get(1) {
            let text = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            // All three turns should be verbatim
            assert!(text.contains("[User]: Turn 1"));
            assert!(text.contains("[User]: Turn 2"));
            assert!(text.contains("[User]: Turn 3"));
            assert!(!text.contains("=== Earlier context"));
        } else {
            panic!("Expected User item");
        }
    }

    #[test]
    fn four_turns_early_summarized_last_three_verbatim() {
        let items = vec![
            system_item("System"),
            user_item("Turn 1"),
            assistant_with_tool_calls("Response 1", &["read_file", "grep"]),
            tool_result("file content"),
            tool_result("search results"),
            user_item("Turn 2"),
            assistant_item("Response 2"),
            user_item("Turn 3"),
            assistant_item("Response 3"),
            user_item("Turn 4"),
            assistant_item("Response 4"),
        ];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 2);

        if let Some(ConversationItem::User(u)) = result.get(1) {
            let text = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            // With 4 turns, the last 3 are verbatim and the first 1 is summarized.
            // early_end = turns[turns.len()-3] = turns[1] = end of turn 2.
            // So turns 1-2 are summarized, turns 3-4 are verbatim.
            assert!(text.contains("=== Earlier context (summarized) ==="));
            // Turns 1 and 2 are summarized: 2 user msgs, 2 assistant msgs
            assert!(
                text.contains("Messages: 2 user, 2 assistant"),
                "Expected summary of turns 1-2. Full text:\n{text}"
            );
            assert!(text.contains("Tools used: grep, read_file"));
            // Last 2 turns (3, 4) should be verbatim
            assert!(text.contains("=== Recent turns (verbatim) ==="));
            assert!(text.contains("[User]: Turn 3"));
            assert!(text.contains("[User]: Turn 4"));
            // Turns 1 and 2 should NOT appear verbatim
            assert!(!text.contains("[User]: Turn 1"));
            assert!(!text.contains("[User]: Turn 2"));
        } else {
            panic!("Expected User item");
        }
    }

    #[test]
    fn normalize_forked_context_with_reasoning_keeps_marker_drops_reasoning() {
        // Multi-turn parent with Reasoning interleaved each turn
        // The distinctive marker must reach the background; reasoning noise must not
        let items = vec![
            system_item("System"),
            user_item("Turn 1 UNIQUE_FORK_MARKER_TEST"),
            reasoning_item("REASONING_NOISE secret chain of thought"),
            assistant_item("Response 1"),
            user_item("Turn 2 follow-up"),
            reasoning_item("REASONING_NOISE more thinking"),
            assistant_item("Response 2"),
        ];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 2);
        assert_eq!(result.len(), 2);

        let text = extract_background_text(
            result
                .get(1)
                .unwrap_or_else(|| panic!("expected background item")),
        );
        assert!(
            text.contains("UNIQUE_FORK_MARKER_TEST"),
            "marker must appear in background: {text}"
        );
        assert!(
            !text.contains("REASONING_NOISE"),
            "reasoning content must be stripped from background: {text}"
        );
    }

    #[test]
    fn project_layout_fully_removed() {
        let items = vec![
            system_item("System"),
            user_item("Before <project_layout>lots of files here</project_layout> After"),
            assistant_item("OK"),
        ];
        let (result, _) = normalize_forked_context(items);
        let text = extract_background_text(
            result
                .get(1)
                .unwrap_or_else(|| panic!("expected background item")),
        );
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
        assert!(!text.contains("lots of files here"));
        assert!(!text.contains("<project_layout>"));
        assert!(!text.contains("</project_layout>"));
    }

    #[test]
    fn tool_result_content_truncated_over_200() {
        let long_content = "x".repeat(300);
        let items = vec![
            system_item("System"),
            user_item("Go"),
            assistant_with_tool_calls("Using tools", &["bash"]),
            tool_result(&long_content),
        ];
        let (result, _) = normalize_forked_context(items);

        if let Some(ConversationItem::User(u)) = result.get(1) {
            let text = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let expected_truncated = format!("{}...", "x".repeat(200));
            assert!(
                text.contains(&expected_truncated),
                "Expected tool result truncated to 200 chars + '...'"
            );
            assert!(!text.contains(&"x".repeat(300)));
        } else {
            panic!("Expected User item");
        }
    }

    #[test]
    fn no_system_item_synthesizes_empty() {
        // Edge case: items start with a User, no System
        let items = vec![user_item("Hello"), assistant_item("Hi")];
        let (result, prefix_len) = normalize_forked_context(items);
        assert_eq!(prefix_len, 2);
        if let Some(ConversationItem::System(s)) = result.first() {
            assert!(s.content.is_empty());
        } else {
            panic!("Expected System item");
        }
    }

    #[test]
    fn tool_call_arguments_truncated_over_100() {
        let long_args = "a".repeat(150);
        let mut item = ConversationItem::assistant("");
        if let ConversationItem::Assistant(ref mut a) = item {
            a.tool_calls = vec![ToolCall {
                id: "tc-1".into(),
                name: "read_file".to_string(),
                arguments: long_args.clone().into(),
            }];
        }
        let items = vec![system_item("System"), user_item("Go"), item];
        let (result, _) = normalize_forked_context(items);

        if let Some(ConversationItem::User(u)) = result.get(1) {
            let text = u
                .content
                .iter()
                .filter_map(|p| match p {
                    distill_sampling_types::conversation::ContentPart::Text { text } => {
                        Some(text.as_ref())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let expected = format!("{}...", "a".repeat(100));
            assert!(
                text.contains(&expected),
                "Expected tool call args truncated to 100 chars + '...'"
            );
            assert!(!text.contains(&long_args));
        } else {
            panic!("Expected User item");
        }
    }

    #[test]
    fn count_complete_turns_basic() {
        let items = [
            user_item("U1"),
            assistant_item("A1"),
            user_item("U2"),
            assistant_item("A2"),
        ];
        let refs: Vec<&ConversationItem> = items.iter().collect();
        let turns = count_complete_turns(&refs);
        assert_eq!(turns.as_slice(), [2, 4]); // after A1, after A2
    }

    #[test]
    fn count_complete_turns_with_tool_results() {
        let items = [
            user_item("U1"),
            assistant_with_tool_calls("A1", &["bash"]),
            tool_result("output"),
            tool_result("output2"),
            user_item("U2"),
            assistant_item("A2"),
        ];
        let refs: Vec<&ConversationItem> = items.iter().collect();
        let turns = count_complete_turns(&refs);
        assert_eq!(turns.as_slice(), [4, 6]); // after 2 tool results, then A2
    }

    #[test]
    fn count_complete_turns_incomplete_trailing() {
        let items = [
            user_item("U1"),
            assistant_item("A1"),
            user_item("U2"), // trailing User with no Assistant
        ];
        let refs: Vec<&ConversationItem> = items.iter().collect();
        let turns = count_complete_turns(&refs);
        assert_eq!(turns.as_slice(), [2]);
    }

    #[test]
    fn count_complete_turns_with_reasoning() {
        // Reasoning siblings sit between the user query and the assistant
        // The counter must see through them or long forked histories never summarize
        let items = [
            user_item("U1"),
            reasoning_item("think 1"),
            assistant_item("A1"),
            user_item("U2"),
            reasoning_item("think 2"),
            assistant_item("A2"),
        ];
        let refs: Vec<&ConversationItem> = items.iter().collect();
        let turns = count_complete_turns(&refs);
        assert_eq!(turns.as_slice(), [3, 6]); // after reasoning+A1, after reasoning+A2
    }

    #[test]
    fn truncate_str_at_limit() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_over_limit() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_zero_length() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_multibyte_emoji() {
        // Each emoji is 4 bytes. Truncating at 2 chars should yield 2 emojis (8 bytes).
        let s = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}"; // 4 emojis
        assert_eq!(truncate_str(s, 2), "\u{1F600}\u{1F601}");
    }

    #[test]
    fn truncate_str_multibyte_cjk() {
        // CJK chars are 3 bytes each. Truncating at 3 chars should yield 3 chars (9 bytes).
        let s = "\u{4F60}\u{597D}\u{4E16}\u{754C}"; // 4 CJK chars
        assert_eq!(truncate_str(s, 3), "\u{4F60}\u{597D}\u{4E16}");
    }

    // --- strip_xml_block tests ---

    #[test]
    fn strip_xml_block_removes_system_reminder() {
        let input = "before <system-reminder>noise content here</system-reminder> after";
        let result = strip_xml_block(input, "system-reminder");
        assert_eq!(result, "before  after");
    }

    #[test]
    fn strip_xml_block_handles_attributes() {
        let input = "before <system-reminder context=\"skills\">noise</system-reminder> after";
        let result = strip_xml_block(input, "system-reminder");
        assert_eq!(result, "before  after");
    }

    #[test]
    fn strip_xml_block_multiple_occurrences() {
        let input =
            "A <system-reminder>one</system-reminder> B <system-reminder>two</system-reminder> C";
        let result = strip_xml_block(input, "system-reminder");
        assert_eq!(result, "A  B  C");
    }

    #[test]
    fn strip_xml_block_no_closing_tag() {
        let input = "before <system-reminder>unclosed content";
        let result = strip_xml_block(input, "system-reminder");
        assert_eq!(result, input, "unclosed tag must be left untouched");
    }

    #[test]
    fn strip_xml_block_false_prefix_match() {
        // "system-reminder-extra" starts with "system-reminder" but is a different tag
        let input = "keep <system-reminder-extra>this</system-reminder-extra>";
        let result = strip_xml_block(input, "system-reminder");
        assert_eq!(result, input);
    }

    // --- strip_skill_instructions tests ---

    #[test]
    fn strip_skill_instructions_preserves_command_metadata() {
        let input = "<user_query>\n\
            <command-name>execute-plan</command-name>\n\
            <command-message>/execute-plan</command-message>\n\
            <command-args>/root/plan.md</command-args> # Skill\n\n\
            You are an orchestrator...\n\n\
            **ARGUMENTS:** /root/plan.md\n\
            </user_query>";
        let result = strip_skill_instructions(input);
        assert!(result.contains("<command-name>execute-plan</command-name>"));
        assert!(result.contains("<command-args>/root/plan.md</command-args>"));
        assert!(result.contains("</user_query>"));
        assert!(!result.contains("orchestrator"));
        assert!(!result.contains("**ARGUMENTS:**"));
    }

    #[test]
    fn strip_skill_instructions_removes_body() {
        let input = "prefix </command-args> skill body here </user_query> suffix";
        let result = strip_skill_instructions(input);
        assert_eq!(result, "prefix </command-args></user_query> suffix");
    }

    #[test]
    fn strip_skill_instructions_no_command_args() {
        let input = "no command args here, just regular text";
        let result = strip_skill_instructions(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_skill_instructions_no_closing_user_query() {
        let input = "prefix </command-args> skill body to end of text";
        let result = strip_skill_instructions(input);
        assert_eq!(result, "prefix </command-args>");
    }

    // --- collapse_blank_lines tests ---

    #[test]
    fn collapse_blank_lines_reduces_runs() {
        let input = "line1\n\n\n\nline2\n\n\nline3";
        let result = collapse_blank_lines(input);
        assert_eq!(result, "line1\n\nline2\n\nline3");
    }

    // --- strip_fork_noise integration tests ---

    #[test]
    fn strip_fork_noise_strips_attached_files() {
        let input = "before <attached_files>\nfn main() {}\n</attached_files> after";
        let result = strip_fork_noise(input);
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(!result.contains("fn main()"));
    }

    #[test]
    fn strip_fork_noise_preserves_user_query() {
        let input = "<user_query>What is 2+2?</user_query>";
        let result = strip_fork_noise(input);
        assert!(result.contains("What is 2+2?"));
    }

    #[test]
    fn strip_fork_noise_realistic_trace() {
        // Synthetic fixture only: keep content generic (no real project docs)
        let input = "\
<system-reminder>\n\
As you answer the user's questions, you can use the following context:\n\n\
## From: /tmp/demo-project/PROJECT.md\n\
# Demo Project\n\n\
## Project Structure\n\
- **src/** - Application sources\n\
- **tests/** - Test suites\n\
</system-reminder>\n\
<user_info>\n\
OS Version: linux\n\
Shell: /bin/bash\n\
Workspace Path: /tmp/demo-project\n\
</user_info>\n\
<git_status>\n\
On branch main\n\
nothing to commit, working tree clean\n\
</git_status>\n\
<project_layout>\n\
src/main.rs\n\
src/lib.rs\n\
Cargo.toml\n\
</project_layout>\n\
<user_query>\n\
<command-name>execute-plan</command-name>\n\
<command-message>/execute-plan</command-message>\n\
<command-args>/tmp/demo-project/plan.md</command-args> # Execute Plan Skill\n\n\
You are an orchestrator that takes a PR Plan DAG and executes it.\n\
This is a very long skill body with many lines of instructions.\n\n\
**ARGUMENTS:** /tmp/demo-project/plan.md\n\
</user_query>";

        let result = strip_fork_noise(input);

        // Command metadata is preserved
        assert!(result.contains("<command-name>execute-plan</command-name>"));
        assert!(result.contains("<command-args>/tmp/demo-project/plan.md</command-args>"));
        // Noise is gone
        assert!(!result.contains("PROJECT.md"));
        assert!(!result.contains("OS Version"));
        assert!(!result.contains("nothing to commit"));
        assert!(!result.contains("src/main.rs"));
        assert!(!result.contains("orchestrator"));
        assert!(!result.contains("**ARGUMENTS:**"));
    }

    // --- normalize_forked_context integration tests ---

    #[test]
    fn normalize_forked_context_empty_after_strip() {
        let items = vec![
            system_item("System"),
            user_item("<system-reminder>pure noise</system-reminder>"),
            assistant_item("Response"),
            user_item("Real question"),
            assistant_item("Real answer"),
        ];
        let (result, _) = normalize_forked_context(items);
        let text = extract_background_text(
            result
                .get(1)
                .unwrap_or_else(|| panic!("expected background item")),
        );
        // The noise-only message should be skipped entirely
        assert!(
            !text.contains("[User]: \n"),
            "empty user line should not appear"
        );
        // Orphaned assistant after stripped user must be retained
        assert!(text.contains("[Assistant]: Response"));
        assert!(text.contains("[User]: Real question"));
    }

    #[test]
    fn normalize_forked_context_mixed_content() {
        let items = vec![
            system_item("System"),
            user_item(
                "<system-reminder>noise</system-reminder>\nWhat is 2+2?\n<user_info>os: linux</user_info>",
            ),
            assistant_item("4"),
        ];
        let (result, _) = normalize_forked_context(items);
        let text = extract_background_text(
            result
                .get(1)
                .unwrap_or_else(|| panic!("expected background item")),
        );
        assert!(text.contains("What is 2+2?"));
        assert!(!text.contains("noise"));
        assert!(!text.contains("os: linux"));
    }

    #[test]
    fn normalize_forked_context_system_reminder_with_attributes() {
        let items = vec![
            system_item("System"),
            user_item(
                "<system-reminder context=\"skills\">\nSkill content\n</system-reminder>\nActual query",
            ),
            assistant_item("answer"),
        ];
        let (result, _) = normalize_forked_context(items);
        let text = extract_background_text(
            result
                .get(1)
                .unwrap_or_else(|| panic!("expected background item")),
        );
        assert!(text.contains("Actual query"));
        assert!(!text.contains("Skill content"));
    }
}
