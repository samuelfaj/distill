// Modified for Distill by Samuel Fajreldines, 2026.
//! Read-only system-block text for `/queue`, `/tasks`, and `/usage`.
//!
//! Plain text committed into scrollback; minimal mode has no interactive panes, so these blocks are its main way to inspect that state.
//! The formatting lives outside `dispatch` so it is easy to unit test.

use crate::app::agent::BgTaskStatus;
use crate::app::agent_view::AgentView;
use crate::app::subagent::format_subagent_label;
use crate::util::{format_duration, group_thousands};

/// `/queue` body: a read-only list of the queued prompts.
/// Rows from the server's shared queue (minus the prompt already running) come first in broadcast order, then the local queue (`pending_prompts`).
/// This matches [`crate::views::queue_pane::QueuePane::sync_from_merged`]'s ordering.
pub(crate) fn queue_block_text(agent: &AgentView) -> String {
    let running_id = agent.session.current_prompt_id.as_deref();

    let mut rows: Vec<String> = Vec::new();
    let mut pos = 1usize;
    for wire in &agent.shared_queue {
        if running_id == Some(wire.id.as_str()) {
            continue;
        }
        rows.push(format_queue_row(pos, &wire.text));
        pos += 1;
    }
    for prompt in &agent.session.pending_prompts {
        rows.push(format_queue_row(pos, &prompt.text));
        pos += 1;
    }

    if rows.is_empty() {
        "Queue is empty.".to_string()
    } else {
        let header = format!(
            "Queued prompt{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

/// `/tasks` body: [`crate::views::tasks_pane::TasksPane`] without its styled rows.
pub(crate) fn tasks_block_text(agent: &AgentView) -> String {
    let mut rows: Vec<String> = Vec::new();

    let mut workflows: Vec<_> = agent.workflow_runs.iter().collect();
    workflows.sort_by(|a, b| {
        b.is_active()
            .cmp(&a.is_active())
            .then(b.received_at.cmp(&a.received_at))
            .then(a.run_id.cmp(&b.run_id))
    });
    for run in workflows {
        let active = run.active_agent_count();
        let agents = match active {
            0 => String::new(),
            1 => " · 1 agent".to_string(),
            n => format!(" · {n} agents"),
        };
        let phase = run
            .current_phase
            .as_deref()
            .map(str::trim)
            .filter(|phase| !phase.is_empty())
            .map(|phase| format!(" · {phase}"))
            .unwrap_or_default();
        rows.push(format!(
            "  {:<9}Workflow · {}{phase}{agents}  ({})",
            if run.is_active() {
                "running".to_string()
            } else {
                run.status.replace('_', " ")
            },
            run.name,
            format_duration(std::time::Duration::from_millis(run.live_elapsed_ms()))
        ));
    }

    // ── Subagents ──
    let mut subs: Vec<_> = agent
        .subagent_sessions
        .values()
        .filter(|s| s.attempt.workflow_run_id.is_none())
        .collect();
    subs.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then(b.attempt.started_at.cmp(&a.attempt.started_at))
            .then(a.child_session_id.cmp(&b.child_session_id))
    });
    for info in subs {
        let (type_label, desc) = format_subagent_label(info);
        let status = if info.attempt.pending_kill {
            "stopping"
        } else if info.is_running() {
            "running"
        } else {
            info.attempt.status.as_deref().unwrap_or("done")
        };
        let label = if desc.is_empty() {
            type_label
        } else {
            format!("{type_label} · {desc}")
        };
        rows.push(format!(
            "  {status:<9}{label}  ({})",
            format_duration(info.display_elapsed())
        ));
    }

    // ── Background tasks / monitors ──
    let mut tasks: Vec<_> = agent.session.bg_tasks.values().collect();
    tasks.sort_by(|a, b| {
        let (ar, br) = (
            a.status == BgTaskStatus::Running,
            b.status == BgTaskStatus::Running,
        );
        br.cmp(&ar)
            .then(b.start_time.cmp(&a.start_time))
            .then(a.task_id.cmp(&b.task_id))
    });
    for task in tasks {
        let kind = if task.is_monitor { "Monitor" } else { "Task" };
        let one_line = task
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_nonempty_line(&task.command));
        let status = if task.pending_kill {
            "stopping"
        } else {
            match task.status {
                BgTaskStatus::Running => "running",
                BgTaskStatus::Done => "done",
                BgTaskStatus::Failed => "failed",
            }
        };
        rows.push(format!(
            "  {status:<9}{kind} · {one_line}  ({})",
            format_duration(task.elapsed())
        ));
    }

    // ── Scheduled (/loop) tasks ──
    let mut sched: Vec<_> = agent.session.scheduled_tasks.values().collect();
    sched.sort_by(|a, b| {
        a.tag
            .cmp(&b.tag)
            .then(a.human_schedule.cmp(&b.human_schedule))
            .then(a.task_id.cmp(&b.task_id))
    });
    for info in sched {
        rows.push(format!(
            "  {:<9}{} · {} · {}",
            "scheduled",
            info.tag,
            info.human_schedule,
            first_nonempty_line(&info.prompt)
        ));
    }

    if rows.is_empty() {
        "No background tasks, workflows, or subagents.".to_string()
    } else {
        let header = format!(
            "Task{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

/// `/usage` body: per-session token and cost totals, covering the ledger's lifetime (since session start, or since the last `/resume`).
pub(crate) fn session_usage_block_text(
    usage: &distill_shell::extensions::notification::PromptUsage,
) -> String {
    let t = &usage.totals;
    if t.model_calls == 0 && usage.model_usage.is_empty() {
        return if usage.usage_is_incomplete {
            "Session usage: none recorded, but tracking is incomplete and may under-count."
                .to_string()
        } else {
            "Session usage: no model calls yet in this session.".to_string()
        };
    }

    let mut rows = Vec::new();
    rows.push(format!(
        "  Input tokens:   {} ({} cached)",
        group_thousands(t.input_tokens),
        group_thousands(t.cached_read_tokens),
    ));
    rows.push(format!(
        "  Output tokens:  {} ({} reasoning)",
        group_thousands(t.output_tokens),
        group_thousands(t.reasoning_tokens),
    ));
    rows.push(format!(
        "  Total tokens:   {}",
        group_thousands(t.total_tokens)
    ));
    rows.push(format!(
        "  Model calls:    {} · API time: {}",
        group_thousands(t.model_calls),
        format_duration(std::time::Duration::from_millis(t.api_duration_ms)),
    ));
    rows.push(format!("  Cost:           {}", format_cost(t)));
    if let Some(cache) = format_cache(t) {
        rows.push(format!("  Cache:          {cache}"));
    }
    if let Some(cache_break) = &usage.last_cache_break {
        rows.push(format!(
            "  Cache break:    {}",
            format_cache_break(
                cache_break.first_changed_index,
                cache_break.changed_kind.as_deref(),
                cache_break.secs_since_previous,
                cache_break.settings_changed,
                cache_break.cache_lifetime_secs,
            )
        ));
    }

    if usage.model_usage.len() > 1 {
        rows.push("  By model:".to_string());
        for (model, m) in &usage.model_usage {
            rows.push(format!(
                "    {model}: {} in / {} out · {}",
                group_thousands(m.input_tokens),
                group_thousands(m.output_tokens),
                format_cost(m),
            ));
            if let Some(cache) = format_cache(m) {
                rows.push(format!("      cache: {cache}"));
            }
        }
    }

    if usage.usage_is_incomplete {
        rows.push("  Note: usage is incomplete and may under-count.".to_string());
    }

    join_header_rows(
        "Session usage (since start or last resume):".to_string(),
        rows,
    )
}

/// Formats the cost cell. Ticks are 1e10 per USD; a partial sum is reported as absent.
fn format_cost(m: &distill_shell::extensions::notification::PromptUsageModel) -> String {
    use distill_shell::extensions::notification::ticks_to_usd;
    match m.cost_usd_ticks {
        Some(ticks) => format!("${:.4}", ticks_to_usd(ticks)),
        None if m.cost_is_partial => "not available (not reported for some calls)".to_string(),
        None => "not available (not reported)".to_string(),
    }
}

/// Cache cell: hit rate over the whole prompt (uncached + read + written, which is what
/// `input_tokens` holds), cache writes split by lifetime (a write without a reported lifetime
/// counts as five-minute), and the estimated saving when prices are known. `None` without cache use.
fn format_cache(m: &distill_shell::extensions::notification::PromptUsageModel) -> Option<String> {
    use distill_shell::extensions::notification::ticks_to_usd;
    if m.input_tokens == 0 || (m.cached_read_tokens == 0 && m.cache_creation_tokens == 0) {
        return None;
    }
    let hit = m.cached_read_tokens as f64 * 100.0 / m.input_tokens as f64;
    let mut parts = vec![format!("{hit:.0}% hit")];
    if m.cache_creation_tokens > 0 {
        let one_hour = m.cache_creation_1h_tokens.min(m.cache_creation_tokens);
        parts.push(if one_hour == 0 {
            format!("{} written (5m)", group_thousands(m.cache_creation_tokens))
        } else {
            format!(
                "{} written ({} 5m / {} 1h)",
                group_thousands(m.cache_creation_tokens),
                group_thousands(m.cache_creation_tokens - one_hour),
                group_thousands(one_hour),
            )
        });
    }
    match m.cache_savings_usd_ticks {
        Some(ticks) if ticks > 0 => parts.push(format!("~${:.4} saved", ticks_to_usd(ticks))),
        Some(ticks) if ticks < 0 => parts.push(format!(
            "~${:.4} lost (writes cost more than reads saved)",
            ticks_to_usd(-ticks)
        )),
        _ => {}
    }
    Some(parts.join(" · "))
}

/// Where the last cache break happened: the first item that changed, the tools or settings when
/// the items were intact, or an expiry when nothing changed and the gap outlived the cache lifetime.
/// An intact request that missed inside its lifetime was changed where the items do not show (the
/// system prompt or settings the sampler sends) or evicted, never expired.
fn format_cache_break(
    first_changed_index: Option<u64>,
    changed_kind: Option<&str>,
    secs_since_previous: u64,
    settings_changed: bool,
    cache_lifetime_secs: Option<u64>,
) -> String {
    let gap = format_duration(std::time::Duration::from_secs(secs_since_previous));
    match (first_changed_index, changed_kind) {
        (Some(index), Some(kind)) => {
            format!("item {index} ({kind}) changed · {gap} after the previous call")
        }
        (Some(index), None) => format!("item {index} changed · {gap} after the previous call"),
        (None, _) if settings_changed => {
            format!("tools or settings changed (items intact) · {gap} after the previous call")
        }
        (None, _) if cache_lifetime_secs.is_some_and(|lifetime| secs_since_previous < lifetime) => {
            format!("items intact, not expired (request changed or evicted) · {gap} after the previous call")
        }
        (None, _) => format!("expired (prefix intact) · {gap} after the previous call"),
    }
}

/// First non-empty, trimmed line of `text` (empty string if none). Collapses a multi-line prompt/command to a single display line.
pub(crate) fn first_nonempty_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Format one `/queue` row as `  #N  <first non-empty line>` with a `(+K more lines)` suffix for multi-line prompts.
fn format_queue_row(pos: usize, text: &str) -> String {
    let first_line = first_nonempty_line(text);
    let extra = text.lines().count().saturating_sub(1);
    if extra > 0 {
        format!(
            "  #{pos}  {first_line}  (+{extra} more line{})",
            if extra == 1 { "" } else { "s" }
        )
    } else {
        format!("  #{pos}  {first_line}")
    }
}

fn join_header_rows(header: String, rows: Vec<String>) -> String {
    std::iter::once(header)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_shell::extensions::notification::{PromptUsage, PromptUsageModel};

    fn model_row(input: u64, output: u64, ticks: Option<i64>) -> PromptUsageModel {
        PromptUsageModel {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            reasoning_tokens: 0,
            model_calls: 1,
            api_duration_ms: 1_000,
            cost_usd_ticks: ticks,
            cost_is_partial: false,
            cost_missing_calls: 0,
            cache_savings_usd_ticks: None,
        }
    }

    #[test]
    fn session_usage_block_empty_ledger() {
        let usage = PromptUsage::default();
        assert_eq!(
            session_usage_block_text(&usage),
            "Session usage: no model calls yet in this session."
        );

        // Empty but incomplete must not read as a clean zero.
        let incomplete = PromptUsage {
            usage_is_incomplete: true,
            ..Default::default()
        };
        assert!(session_usage_block_text(&incomplete).contains("incomplete"));
    }

    #[test]
    fn session_usage_block_formats_tokens_and_cost() {
        let mut totals = model_row(1_234_567, 45_678, Some(12_345_000_000));
        totals.cached_read_tokens = 1_000_000;
        totals.reasoning_tokens = 12_000;
        totals.model_calls = 42;
        totals.api_duration_ms = 192_000;
        let usage = PromptUsage {
            totals,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        // Snapshot pins content and column alignment together; single-model sessions must skip the redundant by-model breakdown
        insta::assert_snapshot!("session_usage_block_full", text);
    }

    #[test]
    fn session_usage_block_lists_models_when_multiple() {
        let mut usage = PromptUsage {
            totals: model_row(150, 15, None),
            ..Default::default()
        };
        usage
            .model_usage
            .insert("Distill".into(), model_row(100, 10, None));
        usage
            .model_usage
            .insert("grok-4".into(), model_row(50, 5, None));
        let text = session_usage_block_text(&usage);
        assert!(text.contains("By model:"), "{text}");
        assert!(text.contains("Distill: 100 in / 10 out"), "{text}");
        assert!(text.contains("grok-4: 50 in / 5 out"), "{text}");
    }

    /// The cache line answers "is the cache paying off": the share of the whole prompt read from
    /// cache, how much was written at each lifetime (1h bills 2x, 5m 1.25x) and, when prices are
    /// known, what the reads saved. Per model too, so a model that keeps missing stands out.
    #[test]
    fn session_usage_block_shows_cache_hit_rate_writes_and_savings() {
        let mut totals = model_row(1_000, 10, Some(1_000));
        totals.cached_read_tokens = 600;
        totals.cache_creation_tokens = 300;
        totals.cache_creation_1h_tokens = 100;
        totals.cache_savings_usd_ticks = Some(27_000_000_000);
        let mut cold = model_row(500, 5, None);
        cold.cache_creation_tokens = 400;
        let mut usage = PromptUsage {
            totals: totals.clone(),
            ..Default::default()
        };
        usage.model_usage.insert("claude".into(), totals);
        usage.model_usage.insert("cold".into(), cold);
        let text = session_usage_block_text(&usage);
        assert!(
            text.contains(
                "  Cache:          60% hit · 300 written (200 5m / 100 1h) · ~$2.7000 saved"
            ),
            "{text}"
        );
        // A write with no reported lifetime counts as five-minute; unknown savings print nothing.
        assert!(
            text.lines()
                .any(|line| line == "      cache: 0% hit · 400 written (5m)"),
            "{text}"
        );
        assert!(!text.contains("Cache break"), "{text}");

        // Net of the write premium: a session whose writes cost more than its reads saved says so.
        usage.totals.cache_savings_usd_ticks = Some(-960_000_000);
        let text = session_usage_block_text(&usage);
        assert!(
            text.contains("~$0.0960 lost (writes cost more than reads saved)"),
            "{text}"
        );
    }

    /// The break line names where the cached prefix stopped matching, or says it expired when the
    /// prefix was intact (the idle gap outlived the cache lifetime).
    #[test]
    fn session_usage_block_shows_the_last_cache_break() {
        let mut totals = model_row(1_000, 10, None);
        totals.cached_read_tokens = 100;
        let mut usage = PromptUsage {
            totals,
            last_cache_break: Some(distill_shell::extensions::notification::CacheBreak {
                first_changed_index: Some(12),
                changed_kind: Some("tool_result".into()),
                prompt_tokens: 50_000,
                cache_read_tokens: 1_000,
                secs_since_previous: 34,
                cache_lifetime_secs: Some(300),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        assert!(
            text.contains(
                "  Cache break:    item 12 (tool_result) changed · 34s after the previous call"
            ),
            "{text}"
        );
        if let Some(cache_break) = usage.last_cache_break.as_mut() {
            cache_break.first_changed_index = None;
            cache_break.changed_kind = None;
            cache_break.secs_since_previous = 400;
        }
        let text = session_usage_block_text(&usage);
        assert!(
            text.contains(
                "  Cache break:    expired (prefix intact) · 6m40s after the previous call"
            ),
            "{text}"
        );
        // Inside the lifetime an intact prefix did not expire: telling the user to wait less would mislead.
        if let Some(cache_break) = usage.last_cache_break.as_mut() {
            cache_break.secs_since_previous = 3;
        }
        let text = session_usage_block_text(&usage);
        assert!(!text.contains("expired (prefix intact)"), "{text}");
        assert!(text.contains("not expired"), "{text}");
        if let Some(cache_break) = usage.last_cache_break.as_mut() {
            cache_break.settings_changed = true;
        }
        let text = session_usage_block_text(&usage);
        assert!(
            text.contains("  Cache break:    tools or settings changed (items intact) · 3.0s"),
            "{text}"
        );
    }

    #[test]
    fn session_usage_block_absent_cost_is_unknown_not_free() {
        let usage = PromptUsage {
            totals: model_row(100, 10, None),
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        insta::assert_snapshot!("session_usage_block_absent_cost", text);
        // Unknown cost must never read as free.
        assert!(!text.contains("$0"), "{text}");
    }

    #[test]
    fn session_usage_block_flags_partial_and_incomplete() {
        let mut totals = model_row(100, 10, None);
        totals.cost_is_partial = true;
        let usage = PromptUsage {
            totals,
            usage_is_incomplete: true,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        assert!(text.contains("not reported for some calls"), "{text}");
        assert!(text.contains("usage is incomplete"), "{text}");
    }

    #[test]
    fn group_thousands_groups_digits() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn first_nonempty_line_skips_blank_leading_lines() {
        assert_eq!(first_nonempty_line("\n  \n  hello \nworld"), "hello");
        assert_eq!(first_nonempty_line("   "), "");
        assert_eq!(first_nonempty_line(""), "");
        assert_eq!(first_nonempty_line("only"), "only");
    }

    #[test]
    fn format_queue_row_single_line() {
        assert_eq!(format_queue_row(1, "fix the bug"), "  #1  fix the bug");
    }

    #[test]
    fn format_queue_row_multiline_reports_extra_lines() {
        assert_eq!(
            format_queue_row(2, "first\nsecond"),
            "  #2  first  (+1 more line)"
        );
        assert_eq!(
            format_queue_row(3, "first\nsecond\nthird"),
            "  #3  first  (+2 more lines)"
        );
    }
}
