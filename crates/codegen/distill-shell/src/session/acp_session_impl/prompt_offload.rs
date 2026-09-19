// Modified for Distill by Samuel Fajreldines, 2026.
//! Large-prompt offload for `SessionActor`: an oversized user turn is written verbatim to the session's
//! `prompts/prompt_{n}.txt` and the model receives a bounded excerpt pointing at it. Every failure path (write error,
//! lost task) still sends a bounded message, never the oversized original; [`OFFLOAD_NOTICE_MARKER`] stays byte-stable for external matchers.

use super::*;
use crate::session::prompt_parser::{ParsedPrompt, PromptLayout};
use distill_telemetry::region;
use distill_telemetry::region::Parent;
use distill_tools::implementations::distill::read_file::{
    MAX_LINES_READ, READ_FILE_MAX_TOKENS, exceeds_read_cap,
};
use distill_tools::types::resources::TruncationCfg;
use distill_tools::types::template_renderer::TemplateRenderer;
use distill_tools::types::tool::ToolKind;
use std::ops::Range;

/// Budget for the inline excerpt: read_file's per-call cap in bytes, so an offloaded prompt's excerpt is never larger than one read would return.
pub(crate) const LARGE_PROMPT_THRESHOLD: usize =
    READ_FILE_MAX_TOKENS * distill_token_estimation::BYTES_PER_TOKEN as usize;

/// Percent of the bounded-prompt budget given to the query (capped; rest is context head).
const LARGE_QUERY_BUDGET_PERCENT: usize = 80;

/// Bytes kept at the TAIL when bounding head+tail, so a trailing question survives.
const BOUNDED_TAIL_BUDGET: usize = 4_000;

/// Floor for the skill and for the context, so neither is erased by an oversized query; leftover budget flows to
/// them afterwards.
const PART_INLINE_FLOOR: usize = 4_000;

/// Fixed part of the notice reserve; the variable parts (path, tool and param names) are added per call.
const NOTICE_BASE_RESERVE: usize = 1_200;

/// Raw bytes per read window: `read_file` caps its formatted output (`N→` anchors), so keep 10 %
/// headroom.
const READ_WINDOW_BYTES: usize = LARGE_PROMPT_THRESHOLD * 9 / 10;

/// Literal `offset`/`limit` pairs the notice lists before it falls back to a continuation clause.
const MAX_NOTICE_WINDOWS: usize = 6;

/// Marker between the head and tail of an elided block. Single source of truth.
const ELISION_MARKER: &str = "\n\n…[middle omitted — see the offload note for how to read it]…\n\n";

/// Stable marker opening the offload notice. Single source of truth (for a future strip-on-re-read).
const OFFLOAD_NOTICE_MARKER: &str = "[Full request offloaded to file]";

/// In-band notice that REPLACES the offload notice when the full request could not be persisted (write error or task-join failure).
/// It references no path, there is no file to read, so the model is never told to `read_file` a file that does not exist.
/// The bounded head+tail excerpt remains.
const OFFLOAD_FAILED_NOTICE: &str = "\n\n[Full request could not be saved to a file — the excerpt above is truncated. Answer from it, and ask the user to resend the full content if anything essential is missing.]";

/// UTF-8-safe suffix: the last `<= max_bytes` bytes of `s`, on a char boundary.
fn truncate_bytes_suffix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // s.len() > max_bytes here; the scan stops at s.len(), which is always a boundary
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Bound `s` to `budget` as HEAD + [`ELISION_MARKER`] + TAIL (trailing question survives), plus the
/// part-relative byte range cut out (`None` when `s` fit whole). UTF-8-safe.
fn bound_head_tail_with_cut(s: &str, budget: usize) -> (String, Option<Range<usize>>) {
    if s.len() <= budget {
        return (s.to_string(), None);
    }
    // Not enough room for a head + marker + tail; fall back to a plain head (empty for budget 0)
    if budget <= ELISION_MARKER.len() {
        let head = truncate_bytes(s, budget);
        return (head.to_string(), Some(head.len()..s.len()));
    }
    // budget > marker and tail_len <= content_budget / 2 (no underflow); head + tail < s.len()
    let content_budget = budget - ELISION_MARKER.len();
    let tail_len = BOUNDED_TAIL_BUDGET.min(content_budget / 2);
    let head_len = content_budget - tail_len;
    let head = truncate_bytes(s, head_len);
    let tail = truncate_bytes_suffix(s, tail_len);
    let elided = head.len()..s.len() - tail.len();
    (format!("{head}{ELISION_MARKER}{tail}"), Some(elided))
}

/// Keep the head of `s` within `budget`, plus the part-relative byte range cut off the end.
/// UTF-8-safe.
fn truncate_head_with_cut(s: &str, budget: usize) -> (&str, Option<Range<usize>>) {
    let kept = truncate_bytes(s, budget);
    if kept.len() < s.len() {
        (kept, Some(kept.len()..s.len()))
    } else {
        (kept, None)
    }
}

/// 1-based line of `byte_idx` in `full`, counted on bytes so an index inside a multibyte char
/// cannot panic.
fn line_of(full: &[u8], byte_idx: usize) -> usize {
    1 + full.iter().take(byte_idx).filter(|&&b| b == b'\n').count()
}

/// One `read_file` call: 1-based first line and line count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadWindow {
    offset: usize,
    limit: usize,
}

/// A part of the request the excerpt does not show, as lines of the offloaded file in `read_file`'s
/// numbering, with the windows that fetch exactly those lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ElidedRange {
    label: &'static str,
    first_line: usize,
    last_line: usize,
    windows: Vec<ReadWindow>,
}

/// Split lines `first_line..=last_line` of `full` into windows of at most `max_lines` lines and
/// about [`READ_WINDOW_BYTES`] raw bytes; a single longer line forms its own window. Sized on whole
/// lines, which is what a read returns, so an elided range is measured snapped outward to line
/// boundaries.
fn read_windows(
    full: &str,
    first_line: usize,
    last_line: usize,
    max_lines: usize,
) -> Vec<ReadWindow> {
    let mut windows = Vec::new();
    if first_line > last_line {
        return windows;
    }
    // A zero cap would never close a window by line count; treat it as one line per window
    let max_lines = max_lines.max(1);
    let mut offset = first_line;
    let mut limit = 0;
    let mut bytes = 0;
    for line in full
        .split_inclusive('\n')
        .skip(first_line.saturating_sub(1))
        .take(last_line.saturating_sub(first_line).saturating_add(1))
    {
        if limit > 0 && (limit == max_lines || bytes + line.len() > READ_WINDOW_BYTES) {
            windows.push(ReadWindow { offset, limit });
            offset = offset.saturating_add(limit);
            limit = 0;
            bytes = 0;
        }
        limit += 1;
        bytes += line.len();
    }
    if limit > 0 {
        windows.push(ReadWindow { offset, limit });
    }
    windows
}

/// Client-facing name of the Read tool and its line-window params as the finalized toolset exposes
/// them (`None` when absent: the notice names nothing it cannot vouch for), plus the session's line
/// cap. The path param is not carried: it differs per toolset and is unambiguous next to the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadToolInfo {
    tool: Option<String>,
    offset: Option<String>,
    limit: Option<String>,
    max_lines: usize,
}

impl Default for ReadToolInfo {
    fn default() -> Self {
        ReadToolInfo {
            tool: None,
            offset: None,
            limit: None,
            max_lines: MAX_LINES_READ,
        }
    }
}

/// Bounded in-band message, the exact notice embedded in it (so a failure path strips the same
/// bytes), and the file line ranges the message does not show.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundedPrompt {
    message: String,
    notice: String,
    elided: Vec<ElidedRange>,
}

/// How to fetch what the excerpt omits, naming only what the toolset actually exposes; with elided
/// ranges it lists their file lines, and the exact read windows when both window params are known.
fn read_guidance(info: &ReadToolInfo, elided: &[ElidedRange]) -> String {
    let ReadToolInfo {
        tool,
        offset,
        limit,
        max_lines,
    } = info;
    if elided.is_empty() {
        let source = match (tool, offset, limit) {
            (Some(tool), Some(offset), Some(limit)) => format!(
                "with `{tool}` using `{offset}` and `{limit}` (in windows of up to \
{max_lines} lines)"
            ),
            (Some(tool), _, _) => format!("with `{tool}`"),
            (None, _, _) => "from that file".to_owned(),
        };
        return format!(
            "Read only the parts you still need {source} rather than re-reading text already in \
this message."
        );
    }
    let ranges = elided
        .iter()
        .map(|range| {
            let ElidedRange {
                label,
                first_line,
                last_line,
                ..
            } = range;
            if first_line == last_line {
                format!("{label} line {first_line}")
            } else {
                format!("{label} lines {first_line}–{last_line}")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    let source = match (tool, offset, limit) {
        (Some(tool), Some(offset), Some(limit)) => {
            let all_windows: Vec<ReadWindow> = elided
                .iter()
                .flat_map(|range| range.windows.iter().copied())
                .collect();
            let mut windows = all_windows
                .iter()
                .take(MAX_NOTICE_WINDOWS)
                .map(|w| format!("{offset}={}, {limit}={}", w.offset, w.limit))
                .collect::<Vec<_>>()
                .join("; ");
            if let Some(next) = all_windows.get(MAX_NOTICE_WINDOWS) {
                windows.push_str(&format!(
                    "; then the remaining listed lines starting at line {}, in windows of up to \
{max_lines} lines",
                    next.offset
                ));
            }
            format!("with `{tool}` — {windows}")
        }
        (Some(tool), _, _) => format!("with `{tool}`"),
        (None, _, _) => "from that file".to_owned(),
    };
    format!(
        "Lines of that file not in this message: {ranges}. Read only those lines {source} — \
everything else is already in this message."
    )
}

/// Build the offload notice: the marker, the path to the file with the user's full request, and
/// how to read only what this message omits (the exact file lines and read windows when parts were
/// cut). Position-neutral wording: the cursor layout places it before the query block.
fn build_offload_notice(
    full_message_len: usize,
    total_lines: usize,
    file_path: &std::path::Path,
    info: &ReadToolInfo,
    elided: &[ElidedRange],
) -> String {
    let guidance = read_guidance(info, elided);
    format!(
        "\n\n{OFFLOAD_NOTICE_MARKER} This message contains only an excerpt of the user's request; \
the complete request ({full_message_len} bytes, {total_lines} lines) is saved verbatim in:\n{}\n\
{guidance} \
Do not mention this file, the excerpt, or the omission to the user unless they ask \
or you could not read a part you needed.",
        file_path.display(),
    )
}

/// Upper bound on the notice length. The excerpt budget is derived from this reserve rather than
/// from the rendered notice so the notice may later describe the excerpt itself (the elided line
/// ranges, each with its own `offset`/`limit` pair, up to six, plus a continuation clause) without
/// the budget becoming circular; the 8× name factor and the base cover that fuller text.
fn notice_reserve(file_path: &std::path::Path, info: &ReadToolInfo) -> usize {
    let name_len = |name: &Option<String>| name.as_deref().map_or(0, str::len);
    NOTICE_BASE_RESERVE
        + file_path.to_string_lossy().len()
        + 8 * (name_len(&info.tool) + name_len(&info.offset) + name_len(&info.limit))
}

/// Build the bounded in-band message for an oversized prompt (`full_message`, laid out per
/// `layout`) already written to `file_path`. Pure; preserves message ordering, stays within budget.
/// The query comes first and yields 20 % only when the skill or the context would otherwise be
/// starved; skill and context keep [`PART_INLINE_FLOOR`] each, and leftover flows to the skill,
/// then the context.
fn build_truncated_prompt_message(
    context: &str,
    query: &str,
    skill_information: &str,
    is_cursor: bool,
    file_path: &std::path::Path,
    full_message: &str,
    layout: &PromptLayout,
    info: &ReadToolInfo,
) -> BoundedPrompt {
    let reserve = notice_reserve(file_path, info);

    // Joiners the layout arms emit: "\n" before the skill in the query block; "\n\n" before the
    // context, which the cursor arm always emits.
    let skill_joiner = if skill_information.is_empty() { 0 } else { 1 };
    let context_joiner = if is_cursor || !context.is_empty() {
        2
    } else {
        0
    };
    let budget = LARGE_PROMPT_THRESHOLD
        .saturating_sub(reserve)
        .saturating_sub(skill_joiner)
        .saturating_sub(context_joiner);

    let skill_floor = PART_INLINE_FLOOR.min(skill_information.len()).min(budget);
    let context_floor = PART_INLINE_FLOOR
        .min(context.len())
        .min(budget.saturating_sub(skill_floor));
    let query_max = budget
        .saturating_sub(skill_floor)
        .saturating_sub(context_floor);
    let mut query_budget = query.len().min(query_max);
    // Only when the other parts would be starved does the query give up 20 %.
    let overflow = skill_information.len().saturating_sub(skill_floor)
        + context.len().saturating_sub(context_floor);
    if overflow > query_max.saturating_sub(query_budget) {
        query_budget = query_budget.min(query_max * LARGE_QUERY_BUDGET_PERCENT / 100);
    }
    let (query_inline, query_cut) = bound_head_tail_with_cut(query, query_budget);

    // left >= skill_floor + context_floor because query_inline.len() <= query_max
    let left = budget.saturating_sub(query_inline.len());
    let skill_budget = skill_information
        .len()
        .min(left.saturating_sub(context_floor));
    let (skill_inline, skill_cut) = bound_head_tail_with_cut(skill_information, skill_budget);

    let left = left.saturating_sub(skill_inline.len());
    let (context_inline, context_cut) = truncate_head_with_cut(context, left);

    // The file holds exactly `full_message` and each cut lies within its part (`*_with_cut`)
    let elided_range = |label: &'static str, part: &Range<usize>, cut: Range<usize>| {
        if cut.is_empty() {
            return None;
        }
        let bytes = full_message.as_bytes();
        let first_line = line_of(bytes, part.start.saturating_add(cut.start));
        let last_byte = part.start.saturating_add(cut.end).saturating_sub(1);
        let last_line = line_of(bytes, last_byte);
        let windows = read_windows(full_message, first_line, last_line, info.max_lines);
        Some(ElidedRange {
            label,
            first_line,
            last_line,
            windows,
        })
    };
    let mut elided = Vec::new();
    elided.extend(query_cut.and_then(|cut| elided_range("user query", &layout.query, cut)));
    if let (Some(part), Some(cut)) = (layout.skill.as_ref(), skill_cut) {
        elided.extend(elided_range("skill instructions", part, cut));
    }
    if let (Some(part), Some(cut)) = (layout.context.as_ref(), context_cut) {
        elided.extend(elided_range("attached context", part, cut));
    }
    elided.sort_by_key(|range| range.first_line);

    let total_lines = full_message.matches('\n').count() + 1;
    // notice.len() <= reserve is pinned by `notice_fits_reserve_with_long_names`
    let notice = build_offload_notice(full_message.len(), total_lines, file_path, info, &elided);

    let query_block = if skill_inline.is_empty() {
        query_inline
    } else {
        format!("{query_inline}\n{skill_inline}")
    };

    let message = if is_cursor {
        format!("{context_inline}{notice}\n\n{query_block}")
    } else if context_inline.is_empty() {
        format!("{query_block}{notice}")
    } else {
        format!("{query_block}\n\n{context_inline}{notice}")
    };
    BoundedPrompt {
        message,
        notice,
        elided,
    }
}

/// Replace the file-referencing offload `notice` embedded in `message` with the no-file [`OFFLOAD_FAILED_NOTICE`].
/// A failed offload therefore never leaves the model chasing a "read this file" pointer to a file that does not exist.
/// Returns `message` unchanged if the notice is absent (defensive).
fn strip_offload_notice(message: &str, notice: &str) -> String {
    message.replacen(notice, OFFLOAD_FAILED_NOTICE, 1)
}

/// On write failure the bounded message is still returned (never the oversized original that would re-overflow the context window).
/// The failure path swaps the embedded notice for [`OFFLOAD_FAILED_NOTICE`], so the model isn't told to read a file that was never written.
/// The injected `writer` makes this testable without touching the filesystem.
fn write_offload_and_build(
    full_message: &str,
    bounded: BoundedPrompt,
    file_path: std::path::PathBuf,
    writer: impl FnOnce(&std::path::Path, &[u8]) -> std::io::Result<()>,
) -> (String, Option<std::path::PathBuf>) {
    match writer(&file_path, full_message.as_bytes()) {
        Ok(()) => (bounded.message, Some(file_path)),
        Err(e) => {
            tracing::warn!(
                ?e,
                full_bytes = full_message.len(),
                "failed to write large-prompt offload file; sending bounded preview with no file reference"
            );
            (
                strip_offload_notice(&bounded.message, &bounded.notice),
                None,
            )
        }
    }
}

impl SessionActor {
    /// Client-facing Read tool and window params as the toolset exposes them (`None` when it does
    /// not), and the session's line cap (the built-in cap without a truncation config). One registry
    /// lock, released before the offload write.
    async fn resolve_read_tool_info(&self) -> ReadToolInfo {
        let bridge = std::sync::Arc::clone(self.agent.borrow().tool_bridge());
        let toolset = bridge.toolset();
        let res = toolset.resources.lock().await;
        let renderer = res.get::<TemplateRenderer>();
        ReadToolInfo {
            tool: renderer
                .and_then(|r| r.tool_for_kind(ToolKind::Read))
                .map(str::to_owned),
            offset: renderer
                .and_then(|r| r.param_for_kind(ToolKind::Read, "offset"))
                .map(str::to_owned),
            limit: renderer
                .and_then(|r| r.param_for_kind(ToolKind::Read, "limit"))
                .map(str::to_owned),
            max_lines: res
                .get::<TruncationCfg>()
                .map_or(MAX_LINES_READ, |t| t.0.max_lines_read()),
        }
    }

    /// If the prompt exceeds `read_file`'s per-call cap (`exceeds_read_cap`), write the full content to a file.
    /// Return a bounded excerpt with the local path embedded for the model to read the rest.
    /// Returns `(assembled_message, Some(local_path))` when offloaded, or `(assembled, None)`.
    pub(super) async fn maybe_truncate_large_prompt_with_skills(
        &self,
        context: String,
        query: String,
        skill_information: String,
        is_cursor: bool,
        prompt_index: usize,
    ) -> (String, Option<std::path::PathBuf>) {
        let (full_message, layout) =
            ParsedPrompt::assemble_with_layout(&context, &query, &skill_information, is_cursor);

        if !exceeds_read_cap(&full_message) {
            return (full_message, None);
        }

        let file_path = get_prompt_file_path(&self.session_info, prompt_index);
        let info = self.resolve_read_tool_info().await;

        // Build the bounded preview once (pure, always within budget) so every outcome (write ok, write fail, lost task) sends a bounded message
        // None of them may send the oversized original that would re-overflow the model context
        let full_len = full_message.len();
        let bounded = build_truncated_prompt_message(
            &context,
            &query,
            &skill_information,
            is_cursor,
            &file_path,
            &full_message,
            &layout,
            &info,
        );
        tracing::debug!(
            full_bytes = full_len,
            elided_ranges = bounded.elided.len(),
            "offloading large prompt to file"
        );
        // The join-failure fallback must also carry no dangling file reference
        // The file may not have been written if the task never ran to completion
        let join_fallback = strip_offload_notice(&bounded.message, &bounded.notice);

        // 0600 via the secure-file helper; on a blocking thread so the large write doesn't stall the executor
        let offload_span = region!("turn.prompt_offload_write", Parent::Inherit);
        let offload = tokio::task::spawn_blocking(move || {
            write_offload_and_build(
                &full_message,
                bounded,
                file_path,
                crate::util::secure_file::write_secure_file,
            )
        })
        .await;
        offload_span.close();
        match offload {
            Ok(result) => result,
            Err(e) => {
                // Task panicked or runtime is tearing down: send the bounded preview (within budget), not the oversized original
                tracing::warn!(
                    ?e,
                    full_bytes = full_len,
                    "spawn_blocking join failed for large-prompt offload"
                );
                (join_fallback, None)
            }
        }
    }
}

#[cfg(test)]
#[path = "prompt_offload_tests.rs"]
mod tests;
