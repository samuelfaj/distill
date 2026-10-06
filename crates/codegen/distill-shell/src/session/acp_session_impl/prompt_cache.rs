// Modified for Distill by Samuel Fajreldines, 2026.
//! Main-request prompt-cache signals: when a round should ask for the one-hour
//! cache lifetime, and a light trace that names where a cache read broke.

use std::hash::Hasher;
use std::time::Instant;

use distill_sampling_types::{ConversationItem, TokenUsage, ToolCall};

/// Whether a wait longer than the default five-minute cache lifetime likely
/// follows this request: the round it answers blocked on a task or subagent,
/// or started work in the background (the model usually waits on it next).
/// Measured: most main-call cache misses on Messages came right after such a
/// wait. Only the last round counts: an older background start would keep
/// every later round on the 2x one-hour write for the rest of the turn.
pub(super) fn long_wait_likely(items: &[ConversationItem]) -> bool {
    let turn_start = items
        .iter()
        .rposition(|item| {
            matches!(item, ConversationItem::User(user) if user.synthetic_reason.starts_prompt_turn())
        })
        .map_or(0, |at| at + 1);
    let Some(last_round) = items
        .get(turn_start..)
        .unwrap_or_default()
        .iter()
        .rev()
        .find_map(|item| match item {
            ConversationItem::Assistant(assistant) => Some(&assistant.tool_calls),
            _ => None,
        })
    else {
        return false;
    };
    last_round
        .iter()
        .any(|call| awaits_long_work(call) || starts_background_work(call))
}

fn arguments(call: &ToolCall) -> serde_json::Value {
    serde_json::from_str(&call.arguments).unwrap_or_default()
}

fn is_spawn(name: &str) -> bool {
    distill_tools::is_task_tool_id(name) || name == "Agent"
}

/// A lenient flag: tools accept `true` and `"true"` alike.
fn flag(args: &serde_json::Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

/// A blocking wait on background work (the task/subagent output tools take a
/// renameable `get_*_output` name and wait when given a positive timeout), or
/// a foreground subagent spawn, which blocks until the child finishes.
fn awaits_long_work(call: &ToolCall) -> bool {
    let name = call.name.as_str();
    if name.starts_with("wait_") || matches!(name, "Await" | "AwaitShell") {
        return true;
    }
    if name.starts_with("get_") && name.ends_with("_output") {
        return distill_tool_types::task_output_waits_from_json(&arguments(call));
    }
    is_spawn(name) && flag(&arguments(call), "run_in_background") == Some(false)
}

/// A subagent spawned in the background (the default) or a command started
/// in the background.
fn starts_background_work(call: &ToolCall) -> bool {
    let args = arguments(call);
    if is_spawn(&call.name) {
        return flag(&args, "run_in_background") != Some(false);
    }
    flag(&args, "is_background") == Some(true)
}

/// A cache read below this share of the previous call's prompt is a break,
/// not the usual tip growth.
const BREAK_READ_SHARE: f64 = 0.5;
/// Prompts smaller than this are under every model's minimum cacheable length.
const MIN_TRACED_PROMPT_TOKENS: u32 = 4_096;

/// Per-item hashes of the last main Messages request, to name the first item
/// that changed when a later call reads far less from cache than it sent.
/// Hashes and kinds only, never content.
#[derive(Debug, Default)]
pub(crate) struct PrefixTrace {
    last: Option<SentPrefix>,
}

#[derive(Debug)]
pub(crate) struct SentPrefix {
    hashes: Vec<u64>,
    kinds: Vec<&'static str>,
    prompt_tokens: u32,
    sent_at: Instant,
}

impl SentPrefix {
    pub(super) fn of(items: &[ConversationItem]) -> Self {
        Self {
            hashes: items.iter().map(item_hash).collect(),
            kinds: items.iter().map(item_kind).collect(),
            prompt_tokens: 0,
            sent_at: Instant::now(),
        }
    }
}

fn item_hash(item: &ConversationItem) -> u64 {
    struct HashWriter(std::collections::hash_map::DefaultHasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Default::default());
    let _ = serde_json::to_writer(&mut writer, item);
    writer.0.finish()
}

fn item_kind(item: &ConversationItem) -> &'static str {
    match item {
        ConversationItem::System(_) => "system",
        ConversationItem::User(_) => "user",
        ConversationItem::Assistant(_) => "assistant",
        ConversationItem::ToolResult(_) => "tool_result",
        ConversationItem::BackendToolCall(_) => "backend_tool_call",
        ConversationItem::Reasoning(_) => "reasoning",
    }
}

impl PrefixTrace {
    /// Records a main Messages call and its usage. Returns what to log when its
    /// cache read fell far below the previous call's prompt: the first item
    /// that differs from the previous request (none: the prefix was intact, so
    /// the entry expired or was evicted) and the gap since that request.
    pub(crate) fn observe(
        &mut self,
        mut sent: SentPrefix,
        usage: &TokenUsage,
    ) -> Option<serde_json::Value> {
        sent.prompt_tokens = usage.prompt_tokens;
        let previous = self.last.replace(sent);
        let previous = previous?;
        let current = self.last.as_ref()?;
        if previous.prompt_tokens < MIN_TRACED_PROMPT_TOKENS
            || f64::from(usage.cached_prompt_tokens)
                >= f64::from(previous.prompt_tokens) * BREAK_READ_SHARE
        {
            return None;
        }
        let first_changed = previous
            .hashes
            .iter()
            .zip(&current.hashes)
            .position(|(before, after)| before != after)
            .or_else(|| {
                (current.hashes.len() < previous.hashes.len()).then_some(current.hashes.len())
            });
        Some(serde_json::json!({
            "first_changed_index": first_changed,
            "kind_before": first_changed.and_then(|at| previous.kinds.get(at).copied()),
            "kind_after": first_changed.and_then(|at| current.kinds.get(at).copied()),
            "items_before": previous.hashes.len(),
            "items_after": current.hashes.len(),
            "previous_prompt_tokens": previous.prompt_tokens,
            "prompt_tokens": usage.prompt_tokens,
            "cache_read_tokens": usage.cached_prompt_tokens,
            "cache_write_tokens": usage.cache_creation_prompt_tokens,
            "secs_since_previous": current.sent_at.duration_since(previous.sent_at).as_secs(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use distill_sampling_types::SyntheticReason;

    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}").into(),
            name: name.to_owned(),
            arguments: arguments.to_string().into(),
        }
    }

    fn round(calls: Vec<ToolCall>) -> Vec<ConversationItem> {
        let results = calls
            .iter()
            .map(|c| ConversationItem::tool_result(c.id.to_string(), "ok"))
            .collect::<Vec<_>>();
        let mut items = vec![ConversationItem::assistant_tool_calls(calls)];
        items.extend(results);
        items
    }

    fn turn(rounds: Vec<Vec<ToolCall>>) -> Vec<ConversationItem> {
        let mut items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("go"),
        ];
        for calls in rounds {
            items.extend(round(calls));
        }
        items
    }

    fn read() -> ToolCall {
        call("read_file", serde_json::json!({"target_file": "a.rs"}))
    }

    /// The round after a blocking wait or a foreground spawn usually waits
    /// again (a poll loop, the next child), and that wait can outlast five
    /// minutes: the request before it must carry the hour.
    #[test]
    fn a_round_that_blocked_on_work_asks_for_the_hour() {
        for blocking in [
            call(
                "get_command_or_task_output",
                serde_json::json!({"task_ids": ["t"], "timeout_ms": 600_000}),
            ),
            call("wait_tasks", serde_json::json!({"task_ids": ["t"]})),
            call(
                "spawn_subagent",
                serde_json::json!({"prompt": "p", "run_in_background": false}),
            ),
        ] {
            let name = blocking.name.clone();
            assert!(
                long_wait_likely(&turn(vec![vec![read()], vec![blocking]])),
                "{name}"
            );
        }
        // A status poll does not block.
        let poll = call(
            "get_command_or_task_output",
            serde_json::json!({"task_ids": ["t"]}),
        );
        assert!(!long_wait_likely(&turn(vec![vec![poll]])));
    }

    /// A round that starts background work is usually followed by a wait on
    /// it; later ordinary rounds go back to the cheaper five-minute write.
    #[test]
    fn background_work_asks_for_the_hour_only_in_the_round_that_started_it() {
        let spawn = call("spawn_subagent", serde_json::json!({"prompt": "p"}));
        let server = call(
            "run_terminal_command",
            serde_json::json!({"command": "make", "is_background": "true"}),
        );
        for started in [spawn, server] {
            assert!(long_wait_likely(&turn(vec![vec![read()], vec![started.clone()]])));
            assert!(
                !long_wait_likely(&turn(vec![vec![started], vec![read()], vec![read()]])),
                "an older start must not keep the rest of the turn on the 2x one-hour write"
            );
        }
        assert!(!long_wait_likely(&turn(vec![vec![read()], vec![read()]])));
    }

    /// A finished wait from an earlier round, or anything from an earlier
    /// prompt turn, says nothing about the wait ahead.
    #[test]
    fn earlier_waits_and_earlier_turns_do_not_count() {
        let wait = call("wait_tasks", serde_json::json!({}));
        assert!(!long_wait_likely(&turn(vec![
            vec![wait.clone()],
            vec![read()]
        ])));

        let mut items = turn(vec![vec![call(
            "spawn_subagent",
            serde_json::json!({"prompt": "p"}),
        )]]);
        items.push(ConversationItem::user("next question"));
        assert!(!long_wait_likely(&items));
        items.extend(round(vec![read()]));
        assert!(!long_wait_likely(&items));

        // A mid-turn reminder is not a new turn.
        let mut items = turn(vec![vec![call(
            "spawn_subagent",
            serde_json::json!({"prompt": "p"}),
        )]]);
        let mut reminder = ConversationItem::user("reminder");
        if let ConversationItem::User(user) = &mut reminder {
            user.synthetic_reason = SyntheticReason::SystemReminder;
        }
        items.push(reminder);
        assert!(long_wait_likely(&items));
    }

    fn usage(prompt: u32, cached: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            cached_prompt_tokens: cached,
            ..Default::default()
        }
    }

    /// The trace exists to find the remaining cache breaks: when a call reads
    /// far less than the previous call sent, it names the first item that
    /// differs (an old item rewritten), or none when the prefix was intact
    /// (the entry expired). Normal tip growth logs nothing.
    #[test]
    fn a_broken_read_names_the_first_changed_item() {
        let mut trace = PrefixTrace::default();
        let history = turn(vec![vec![read()], vec![read()], vec![read()]]);
        assert!(
            trace
                .observe(SentPrefix::of(&history), &usage(50_000, 0))
                .is_none(),
            "first call"
        );

        let mut grown = history.clone();
        grown.extend(round(vec![read()]));
        assert!(
            trace
                .observe(SentPrefix::of(&grown), &usage(52_000, 49_000))
                .is_none(),
            "warm"
        );

        let mut edited = grown.clone();
        edited[3] =
            ConversationItem::tool_result("call-read_file", "[Tool result omitted — too old]");
        let report = trace
            .observe(SentPrefix::of(&edited), &usage(52_000, 1_000))
            .expect("a break is reported");
        assert_eq!(report["first_changed_index"], 3);
        assert_eq!(report["kind_before"], "tool_result");
        assert!(
            !report.to_string().contains("too old"),
            "no content in the report"
        );

        let report = trace
            .observe(SentPrefix::of(&edited), &usage(52_000, 0))
            .expect("an intact prefix that missed is reported");
        assert!(report["first_changed_index"].is_null(), "{report}");
    }
}
