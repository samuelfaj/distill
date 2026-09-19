// Modified for Distill by Samuel Fajreldines, 2026.
use super::super::support::{create_test_actor, test_agent_with_tools};
use super::*;
use distill_tools::implementations::distill::read_file::MAX_LINES_READ;
use distill_tools::registry::types::ToolConfig;
use distill_tools::types::context::TruncationConfig;
use distill_tools::types::resources::TruncationCfg;
const HEAD_TOKEN: &str = "HEADSTART_TOKEN_aaa";
const TAIL_TOKEN: &str = "TAILEND_TOKEN_zzz";
fn fake_prompt_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/grok-test-home/sessions/cwd/sid/prompts/prompt_0.txt")
}
/// What the real distill toolset resolves to.
fn distill_info() -> ReadToolInfo {
    ReadToolInfo {
        tool: Some("read_file".to_owned()),
        offset: Some("offset".to_owned()),
        limit: Some("limit".to_owned()),
        max_lines: MAX_LINES_READ,
    }
}
/// The bounded message for the distill read tool info, over the parts' own assembly.
fn bounded_message(
    context: &str,
    query: &str,
    skill_information: &str,
    is_cursor: bool,
    file_path: &std::path::Path,
) -> String {
    let (full, layout) =
        ParsedPrompt::assemble_with_layout(context, query, skill_information, is_cursor);
    build_truncated_prompt_message(
        context,
        query,
        skill_information,
        is_cursor,
        file_path,
        &full,
        &layout,
        &distill_info(),
    )
    .message
}
/// `truncate_bytes_suffix` keeps a suffix that starts on a UTF-8 char boundary, so multibyte text never splits.
#[test]
fn truncate_bytes_suffix_is_utf8_safe() {
    assert_eq!(truncate_bytes_suffix("hello", 5), "hello");
    assert_eq!(truncate_bytes_suffix("hello world", 5), "world");
    let s = "a🎉🎉b";
    let out = truncate_bytes_suffix(s, 6);
    assert!(out.len() <= 6);
    assert!(s.ends_with(out));
    assert!(std::str::from_utf8(out.as_bytes()).is_ok());
}
/// `bound_head_tail_with_cut` returns the input when it fits, else the head, the elision marker,
/// and the tail within budget.
#[test]
fn bound_head_tail_boundary_and_utf8() {
    let fits = "a".repeat(100);
    assert_eq!(bound_head_tail_with_cut(&fits, 100).0, fits);
    let over = "a".repeat(101);
    let out = bound_head_tail_with_cut(&over, 100).0;
    assert!(
        out.len() <= 100,
        "bounded output ({}) exceeds budget",
        out.len()
    );
    assert!(out.contains(ELISION_MARKER));
    let mb = "🎉".repeat(5_000);
    let out_mb = bound_head_tail_with_cut(&mb, 8_000).0;
    assert!(out_mb.len() <= 8_000);
    assert!(out_mb.starts_with('🎉'));
    assert!(out_mb.ends_with('🎉'));
}
/// The cut is exactly the bytes between the shown head and tail, on char boundaries.
#[test]
fn bound_head_tail_with_cut_reconstructs_output() {
    let fits = "a".repeat(100);
    assert_eq!((fits.clone(), None), bound_head_tail_with_cut(&fits, 100));
    let ascii = format!("HEAD{}TAIL", "m".repeat(20_000));
    let mb = "🎉".repeat(5_000);
    let tiny_budget = ELISION_MARKER.len();
    for (s, budget) in [(&ascii, 9_000), (&mb, 8_001), (&ascii, tiny_budget)] {
        let (out, cut) = bound_head_tail_with_cut(s, budget);
        let cut = cut.expect("over budget must cut");
        assert!(!cut.is_empty());
        assert!(s.is_char_boundary(cut.start));
        assert!(s.is_char_boundary(cut.end));
        let expected = if budget <= ELISION_MARKER.len() {
            s[..cut.start].to_owned()
        } else {
            format!("{}{ELISION_MARKER}{}", &s[..cut.start], &s[cut.end..])
        };
        assert_eq!(expected, out);
    }
    let (kept, cut) = truncate_head_with_cut(&ascii, 5_000);
    assert_eq!((&ascii[..5_000], Some(5_000..ascii.len())), (kept, cut));
    assert_eq!(
        (ascii.as_str(), None),
        truncate_head_with_cut(&ascii, ascii.len())
    );
}
/// Oversized query: the bounded message keeps a HEAD and a TAIL, so the trailing question survives, and elides the middle.
/// The full body is never inlined.
#[test]
fn build_truncated_keeps_query_head_and_tail() {
    let path = fake_prompt_path();
    let middle = "M".repeat(LARGE_PROMPT_THRESHOLD * 3);
    let query = format!("{HEAD_TOKEN} {middle} {TAIL_TOKEN} what does this say?");
    let message = bounded_message("", &query, "", false, &path);
    assert!(message.contains(HEAD_TOKEN), "head must survive inline");
    assert!(message.contains(TAIL_TOKEN), "tail must survive inline");
    assert!(
        message.contains("what does this say?"),
        "trailing question must survive inline"
    );
    let head_idx = message.find(HEAD_TOKEN).expect("head present");
    let tail_idx = message.find(TAIL_TOKEN).expect("tail present");
    assert!(
        head_idx < tail_idx,
        "head must appear before tail in the bounded inline message"
    );
    assert!(
        !message.contains(&middle),
        "middle bulk must not be inlined"
    );
    assert!(
        message.contains(ELISION_MARKER),
        "elision marker must mark the cut"
    );
    assert!(
        !message.contains(&query),
        "full query body must not be inlined"
    );
    assert!(message.contains(OFFLOAD_NOTICE_MARKER));
    assert!(message.contains(&path.display().to_string()));
    assert!(
        message.len() <= LARGE_PROMPT_THRESHOLD,
        "message ({}) must stay within budget",
        message.len()
    );
}
/// Large context and small query: the query stays intact, the context is truncated.
#[test]
fn build_truncated_preserves_small_query_truncates_context() {
    let path = fake_prompt_path();
    let context = format!("CTXHEAD_TOKEN {}", "C".repeat(LARGE_PROMPT_THRESHOLD * 3));
    let query = "please summarise the attached file".to_string();
    let message = bounded_message(&context, &query, "", false, &path);
    assert!(message.contains(&query), "small query preserved intact");
    assert!(
        message.starts_with(&query),
        "grok ordering: query block first"
    );
    assert!(message.contains("CTXHEAD_TOKEN"), "context head preserved");
    assert!(!message.contains(&context), "oversized context truncated");
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
}
/// Both query and context oversized (the 80/20 split arm): both are bounded and neither full body is inlined.
#[test]
fn build_truncated_both_oversized_keeps_bounded_heads() {
    let path = fake_prompt_path();
    let query = format!(
        "QHEAD_TOKEN {} QTAIL_TOKEN",
        "Q".repeat(LARGE_PROMPT_THRESHOLD * 2)
    );
    let context = format!("CHEAD_TOKEN {}", "C".repeat(LARGE_PROMPT_THRESHOLD * 2));
    let (full, layout) = ParsedPrompt::assemble_with_layout(&context, &query, "", false);
    let info = ReadToolInfo::default();
    let bounded =
        build_truncated_prompt_message(&context, &query, "", false, &path, &full, &layout, &info);
    let message = &bounded.message;
    assert!(
        message.contains("QHEAD_TOKEN"),
        "bounded query head present"
    );
    assert!(
        message.contains("QTAIL_TOKEN"),
        "bounded query tail present"
    );
    assert!(
        message.contains("CHEAD_TOKEN"),
        "bounded context head present"
    );
    assert!(!message.contains(&query), "full query not inlined");
    assert!(!message.contains(&context), "full context not inlined");
    assert!(
        message.starts_with("QHEAD_TOKEN"),
        "grok ordering: query first"
    );
    assert!(
        message.len() <= LARGE_PROMPT_THRESHOLD,
        "message ({}) must stay within budget",
        message.len()
    );
    let context_start = message.find("CHEAD_TOKEN").expect("context head present");
    let notice_start = message.find(&bounded.notice).expect("notice present");
    assert!(
        notice_start - context_start > PART_INLINE_FLOOR,
        "context inline ({}) must exceed the floor",
        notice_start - context_start
    );
}
/// Compat-harness ordering: the context and the notice come first, the query block last.
#[test]
fn build_truncated_cursor_ordering() {
    let path = fake_prompt_path();
    let query = format!(
        "QHEAD_TOKEN {} QTAIL_TOKEN",
        "Q".repeat(LARGE_PROMPT_THRESHOLD * 2)
    );
    let context = format!("CHEAD_TOKEN {}", "C".repeat(LARGE_PROMPT_THRESHOLD * 2));
    let message = bounded_message(&context, &query, "", true, &path);
    assert!(message.starts_with("CHEAD_TOKEN"), "cursor: context first");
    assert!(
        !message.starts_with("QHEAD_TOKEN"),
        "cursor: query is not first"
    );
    assert!(message.ends_with("QTAIL_TOKEN"), "cursor: query block last");
    let marker_idx = message.find(OFFLOAD_NOTICE_MARKER).expect("notice present");
    let query_idx = message.find("QHEAD_TOKEN").expect("query head present");
    assert!(
        marker_idx < query_idx,
        "cursor: notice precedes the query block"
    );
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
}
/// Skills survive inline even when the query is oversized; they have their own inline budget.
#[test]
fn build_truncated_preserves_skill_information() {
    let path = fake_prompt_path();
    let query = "Q".repeat(LARGE_PROMPT_THRESHOLD * 3);
    let skills = "SKILL_MARKER: follow the xyz skill steps".to_string();
    let message = bounded_message("", &query, &skills, false, &path);
    assert!(
        message.contains("SKILL_MARKER"),
        "invoked-skill text must survive inline even with an oversized query"
    );
    assert!(
        !message.contains(&query),
        "full query body must not be inlined"
    );
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
}
/// A skill over the whole budget is bounded to a head and a tail; the full body is not inlined.
#[test]
fn build_truncated_bounds_oversized_skill_head_and_tail() {
    let path = fake_prompt_path();
    let query = "short query".to_string();
    let skills = format!(
        "SKILLHEAD_TOKEN {} SKILLTAIL_TOKEN",
        "S".repeat(LARGE_PROMPT_THRESHOLD * 2)
    );
    let message = bounded_message("", &query, &skills, false, &path);
    assert!(
        message.contains("SKILLHEAD_TOKEN"),
        "skill head must survive inline"
    );
    assert!(
        message.contains("SKILLTAIL_TOKEN"),
        "skill tail (closing framing) must survive inline"
    );
    assert!(
        !message.contains(&skills),
        "full skill body must not be inlined"
    );
    assert!(
        message.contains(ELISION_MARKER),
        "oversized skill must be marked as elided"
    );
    assert!(message.contains(&query), "small query stays intact");
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
    assert!(
        !message.contains("truncated"),
        "neither the elision marker nor the notice may call the cut a truncation"
    );
}
/// Multibyte query and context: bounding must not panic and stays within budget.
#[test]
fn build_truncated_multibyte_no_panic() {
    let path = fake_prompt_path();
    let query = "路".repeat(LARGE_PROMPT_THRESHOLD);
    let context = "🎉".repeat(LARGE_PROMPT_THRESHOLD);
    let message = bounded_message(&context, &query, "", false, &path);
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
    assert!(message.contains(OFFLOAD_NOTICE_MARKER));
}
/// Numbered lines so any elided line is a unique probe.
fn numbered_lines(prefix: &str, count: usize) -> String {
    (0..count).map(|i| format!("{prefix}{i:06}\n")).collect()
}
/// Every `ElidedRange` names file lines (in `read_file`'s numbering) that hold the cut bytes and
/// nothing the message shows; its windows cover exactly those lines.
fn assert_elided_ranges_match_file(is_cursor: bool) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("prompt_0.txt");
    let query = format!(
        "{HEAD_TOKEN}\n{}{TAIL_TOKEN} what?",
        numbered_lines("Q", 40_000)
    );
    let skills = format!(
        "SKILLHEAD_TOKEN\n{}SKILLTAIL_TOKEN",
        numbered_lines("S", 25_000)
    );
    let context = format!("CHEAD_TOKEN\n{}", numbered_lines("C", 40_000));
    let (full, layout) = ParsedPrompt::assemble_with_layout(&context, &query, &skills, is_cursor);
    std::fs::write(&path, &full).unwrap();
    let info = ReadToolInfo::default();
    let bounded = build_truncated_prompt_message(
        &context, &query, &skills, is_cursor, &path, &full, &layout, &info,
    );
    let file = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = file.split_inclusive('\n').collect();
    let labels: Vec<&str> = bounded.elided.iter().map(|r| r.label).collect();
    let expected = if is_cursor && false {
        vec!["attached context", "user query", "skill instructions"]
    } else {
        vec!["user query", "skill instructions", "attached context"]
    };
    assert_eq!(expected, labels, "one range per part, in file order");
    for pair in bounded.elided.windows(2) {
        let [earlier, later] = pair else {
            unreachable!("windows(2)")
        };
        assert!(earlier.last_line < later.first_line, "disjoint");
    }
    for range in &bounded.elided {
        assert!(range.first_line <= range.last_line);
        assert!(range.last_line <= lines.len());
        let region: String = lines
            .get(range.first_line - 1..range.last_line)
            .expect("range within the file")
            .concat();
        let label = range.label;
        let (probe_prefix, shown): (&str, &[&str]) = match label {
            "user query" => ("Q", &[HEAD_TOKEN, TAIL_TOKEN]),
            "skill instructions" => ("S", &["SKILLHEAD_TOKEN", "SKILLTAIL_TOKEN"]),
            _ => ("C", &["CHEAD_TOKEN"]),
        };
        let interior = lines
            .get(range.first_line..range.last_line - 1)
            .expect("range within the file");
        for &line in interior.iter().step_by(97).chain(interior.last()) {
            assert!(line.starts_with(probe_prefix), "{line:?} in {label}");
            assert!(!bounded.message.contains(line), "{line:?} must be elided");
        }
        for &token in shown {
            assert!(!region.contains(token), "{token} is shown, not elided");
        }
        let covered: usize = range.windows.iter().map(|w| w.limit).sum();
        assert_eq!(range.last_line + 1 - range.first_line, covered);
        assert_eq!(
            Some(range.first_line),
            range.windows.first().map(|w| w.offset)
        );
        for w in &range.windows {
            assert!(0 < w.limit && w.limit <= info.max_lines, "{w:?}");
        }
    }
}
#[test]
fn elided_ranges_match_file_lines_grok() {
    assert_elided_ranges_match_file(false);
}
/// A short query leaves the budget to the skill: an 8 KB skill (twice the floor) is inlined whole.
#[test]
fn build_truncated_short_query_gives_skill_the_leftover() {
    let path = fake_prompt_path();
    let query = "short query".to_string();
    let skills = format!(
        "SKILLHEAD_TOKEN {} SKILLTAIL_TOKEN",
        "S".repeat(PART_INLINE_FLOOR * 2)
    );
    let message = bounded_message("", &query, &skills, false, &path);
    assert!(message.contains(&skills), "skill must be inlined whole");
    assert!(!message.contains(ELISION_MARKER), "nothing was elided");
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
}
/// With no skill and no context the whole budget goes to the query instead of a fixed 80 % share.
#[test]
fn build_truncated_oversized_query_alone_uses_full_budget() {
    let path = fake_prompt_path();
    let query = "Q".repeat(LARGE_PROMPT_THRESHOLD * 3);
    let message = bounded_message("", &query, "", false, &path);
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
    let min_len = LARGE_PROMPT_THRESHOLD - notice_reserve(&path, &distill_info());
    assert!(
        message.len() >= min_len,
        "query must use the whole budget, got {}",
        message.len()
    );
}
/// Skill and context both oversized with a short query: the skill takes the leftover and the context
/// keeps its floor.
#[test]
fn build_truncated_skill_and_context_oversized_keeps_context_floor() {
    let path = fake_prompt_path();
    let query = "short query".to_string();
    let skills = format!(
        "SKILLHEAD_TOKEN {} SKILLTAIL_TOKEN",
        "S".repeat(LARGE_PROMPT_THRESHOLD * 2)
    );
    let context = format!("CHEAD_TOKEN {}", "C".repeat(LARGE_PROMPT_THRESHOLD * 2));
    let message = bounded_message(&context, &query, &skills, false, &path);
    assert!(message.contains("SKILLHEAD_TOKEN"));
    assert!(message.contains("SKILLTAIL_TOKEN"));
    assert!(
        message.contains(&context[..PART_INLINE_FLOOR]),
        "context keeps a head of at least the floor"
    );
    assert!(message.len() <= LARGE_PROMPT_THRESHOLD);
    let min_len = LARGE_PROMPT_THRESHOLD - notice_reserve(&path, &distill_info());
    assert!(
        message.len() >= min_len,
        "skill must take the leftover, got {}",
        message.len()
    );
}
/// A context that already fits its floor does not trigger the 80/20 split, so the query keeps the rest.
#[test]
fn build_truncated_small_context_does_not_trigger_80_20() {
    let path = fake_prompt_path();
    let query = "Q".repeat(LARGE_PROMPT_THRESHOLD * 3);
    let context = format!("CHEAD_TOKEN {}", "C".repeat(3_000));
    let (full, layout) = ParsedPrompt::assemble_with_layout(&context, &query, "", false);
    let info = ReadToolInfo::default();
    let bounded =
        build_truncated_prompt_message(&context, &query, "", false, &path, &full, &layout, &info);
    assert!(bounded.message.contains(&context), "context inlined whole");
    let query_inline_len = bounded.message.len() - bounded.notice.len() - 2 - context.len();
    assert!(
        query_inline_len > LARGE_PROMPT_THRESHOLD * LARGE_QUERY_BUDGET_PERCENT / 100,
        "query kept more than an 80 % share ({query_inline_len})"
    );
    assert!(bounded.message.len() <= LARGE_PROMPT_THRESHOLD);
}
/// The offload notice opens with the marker, substitutes the byte count and the path, and names only
/// what the toolset exposes: tool and window params when all are present, the tool alone when a
/// window param is missing, and no name at all when there is no Read tool.
#[test]
fn build_offload_notice_names_only_what_the_toolset_exposes() {
    let path = fake_prompt_path();
    let all = distill_info();
    let notice = build_offload_notice(123_456, 1, &path, &all, &[]);
    assert!(notice.trim_start().starts_with(OFFLOAD_NOTICE_MARKER));
    assert!(notice.contains("123456 bytes"));
    assert!(notice.contains(&path.display().to_string()));
    assert!(notice.contains("with `read_file` using `offset` and `limit`"));
    assert!(notice.contains(&format!("up to {MAX_LINES_READ} lines")));
    let custom = ReadToolInfo {
        tool: Some("open_file".to_owned()),
        offset: Some("start_line".to_owned()),
        limit: Some("line_count".to_owned()),
        max_lines: 250,
    };
    let notice = build_offload_notice(123_456, 1, &path, &custom, &[]);
    assert!(notice.contains("with `open_file` using `start_line` and `line_count`"));
    assert!(notice.contains("250 lines"));
    let no_limit = ReadToolInfo {
        limit: None,
        ..custom.clone()
    };
    let notice = build_offload_notice(123_456, 1, &path, &no_limit, &[]);
    assert!(notice.contains("with `open_file` rather than"));
    assert!(!notice.contains("start_line"));
    assert!(!notice.contains("250"));
    let notice = build_offload_notice(123_456, 1, &path, &ReadToolInfo::default(), &[]);
    assert!(notice.trim_start().starts_with(OFFLOAD_NOTICE_MARKER));
    assert!(notice.contains("from that file rather than"));
    assert!(!notice.contains('`'), "nothing to vouch for: {notice}");
    assert!(!notice.contains("truncated"));
}
fn window(offset: usize, limit: usize) -> ReadWindow {
    ReadWindow { offset, limit }
}
/// Windows cover the range exactly, never exceed the line cap, and stay under the raw-byte cap
/// unless a single line is longer than it.
fn assert_windows_cover(full: &str, first_line: usize, last_line: usize, max_lines: usize) {
    let windows = read_windows(full, first_line, last_line, max_lines);
    let lines: Vec<&str> = full.split_inclusive('\n').collect();
    let covered: usize = windows.iter().map(|w| w.limit).sum();
    assert_eq!(last_line + 1 - first_line, covered);
    assert_eq!(Some(first_line), windows.first().map(|w| w.offset));
    let mut next = first_line;
    for w in &windows {
        assert_eq!(next, w.offset, "windows are contiguous");
        assert!(0 < w.limit && w.limit <= max_lines, "{w:?}");
        let bytes: usize = lines
            .get(w.offset - 1..w.offset - 1 + w.limit)
            .expect("window within the file")
            .iter()
            .map(|l| l.len())
            .sum();
        assert!(
            w.limit == 1 || bytes <= READ_WINDOW_BYTES,
            "{w:?} spans {bytes} bytes"
        );
        next += w.limit;
    }
}
#[test]
fn read_windows_cover_range_and_respect_caps() {
    let short_lines = "x\n".repeat(1_500);
    assert_eq!(
        vec![window(1, 1_000), window(1_001, 500)],
        read_windows(&short_lines, 1, 1_500, 1_000)
    );
    assert_windows_cover(&short_lines, 1, 1_500, 1_000);
    assert_windows_cover(&short_lines, 37, 1_212, 250);
    let wide_lines = format!("{}\n", "y".repeat(99)).repeat(2_000);
    let windows = read_windows(&wide_lines, 1, 2_000, 1_000);
    assert!(windows.len() >= 3, "{windows:?}");
    assert_windows_cover(&wide_lines, 1, 2_000, 1_000);
    assert_windows_cover(&wide_lines, 500, 1_700, 1_000);
}
/// Degenerate inputs neither panic nor hang: an inverted range is empty, a zero line cap means one
/// line per window, and a zero first line still yields a single covering window.
#[test]
fn read_windows_zero_or_inverted_input_is_empty_or_single() {
    let three = "a\nb\nc\n";
    assert!(read_windows(three, 3, 2, 1_000).is_empty());
    let one_per_line = read_windows(three, 1, 3, 0);
    assert_eq!(3, one_per_line.len());
    assert!(one_per_line.iter().all(|w| w.limit == 1));
    assert_eq!(vec![window(0, 1)], read_windows(three, 0, 0, 1_000));
    assert_eq!(
        4,
        line_of(three.as_bytes(), usize::MAX - 1),
        "index clamped to the end"
    );
}
#[test]
fn read_windows_single_long_line() {
    let long = "z".repeat(200_000);
    assert_eq!(vec![window(1, 1)], read_windows(&long, 1, 1, 1_000));
    let mixed = format!("a\n{long}\nb\n");
    assert_eq!(
        3,
        read_windows(&mixed, 1, 3, 1_000).len(),
        "the long line stands alone"
    );
    assert_windows_cover(&mixed, 1, 3, 1_000);
}
/// `line_of` takes bytes: a range ending right after `🎉` puts `b - 1` inside the char.
#[test]
fn read_windows_multibyte_range_end() {
    let full = "ab🎉\ncd\n";
    let end = full.find('\n').unwrap();
    assert!(!full.is_char_boundary(end - 1));
    assert_eq!(1, line_of(full.as_bytes(), end - 1));
    assert_eq!(2, line_of(full.as_bytes(), end + 1));
    let last_line = line_of(full.as_bytes(), end - 1);
    assert_windows_cover(full, 1, last_line, 1_000);
}
/// The notice lists every range with its lines, at most [`MAX_NOTICE_WINDOWS`] literal windows, and
/// a continuation clause only when windows were left out; without both window params it lists the
/// ranges but no windows.
#[test]
fn notice_lists_ranges_and_windows() {
    let path = fake_prompt_path();
    let info = distill_info();
    let range = |label, first_line, last_line, windows: &[(usize, usize)]| ElidedRange {
        label,
        first_line,
        last_line,
        windows: windows.iter().map(|&(o, k)| window(o, k)).collect(),
    };
    let few = [
        range("user query", 10, 20, &[(10, 11)]),
        range("attached context", 500, 500, &[(500, 1)]),
    ];
    let notice = build_offload_notice(9_999, 600, &path, &info, &few);
    assert!(notice.trim_start().starts_with(OFFLOAD_NOTICE_MARKER));
    assert!(notice.contains("(9999 bytes, 600 lines)"));
    assert!(notice.contains("user query lines 10–20; attached context line 500"));
    assert!(notice.contains("offset=10, limit=11; offset=500, limit=1"));
    assert!(!notice.contains("starting at line"));
    assert!(!notice.contains("truncated"));
    let query_windows = [(1, 1_000), (1_001, 1_000), (2_001, 1_000), (3_001, 1_000)];
    let skill_windows = [(5_000, 1_000), (6_000, 1_000), (7_000, 501)];
    let many = [
        range("user query", 1, 4_000, &query_windows),
        range("skill instructions", 5_000, 7_500, &skill_windows),
    ];
    let notice = build_offload_notice(9_999, 8_000, &path, &info, &many);
    assert_eq!(MAX_NOTICE_WINDOWS, notice.matches("offset=").count());
    assert!(notice.contains("starting at line 7000"));
    assert!(notice.contains("up to 1000 lines"), "session line cap");
    let no_limit = ReadToolInfo {
        tool: Some("Read".to_owned()),
        limit: None,
        ..info.clone()
    };
    let notice = build_offload_notice(9_999, 600, &path, &no_limit, &few);
    assert!(notice.contains("user query lines 10–20; attached context line 500"));
    assert!(notice.contains("with `Read`"));
    assert!(!notice.contains("offset="));
    let notice = build_offload_notice(9_999, 600, &path, &ReadToolInfo::default(), &few);
    assert!(notice.contains("user query lines 10–20; attached context line 500"));
    assert!(notice.contains("from that file"));
    assert!(!notice.contains('`'), "nothing to vouch for: {notice}");
}
/// The notice never outgrows its reserve, so the excerpt budget derived from the reserve holds on every path.
#[test]
fn notice_fits_reserve_with_long_names() {
    let path = fake_prompt_path();
    let defaults = ReadToolInfo::default();
    let notice = build_offload_notice(usize::MAX, 1, &path, &defaults, &[]);
    assert!(notice.len() <= notice_reserve(&path, &defaults));
    let long_path = std::path::PathBuf::from(format!("/{}/prompt_0.txt", "p".repeat(1_000)));
    let long_names = ReadToolInfo {
        tool: Some("T".repeat(64)),
        offset: Some("O".repeat(64)),
        limit: Some("L".repeat(64)),
        max_lines: usize::MAX,
    };
    let notice = build_offload_notice(usize::MAX, 1, &long_path, &long_names, &[]);
    assert!(notice.len() <= notice_reserve(&long_path, &long_names));
    let window = ReadWindow {
        offset: usize::MAX,
        limit: usize::MAX,
    };
    let fullest: Vec<ElidedRange> = ["user query", "skill instructions", "attached context"]
        .into_iter()
        .enumerate()
        .map(|(i, label)| ElidedRange {
            label,
            first_line: usize::MAX,
            last_line: usize::MAX,
            windows: vec![window; 2 + usize::from(i == 0)],
        })
        .collect();
    let grok = distill_info();
    for (file_path, info) in [(&path, &grok), (&long_path, &long_names)] {
        let notice = build_offload_notice(usize::MAX, usize::MAX, file_path, info, &fullest);
        let offset = info.offset.as_deref().expect("window param present");
        let listed = notice.matches(&format!("{offset}=")).count();
        assert_eq!(MAX_NOTICE_WINDOWS, listed);
        assert!(notice.contains("starting at line"));
        assert!(
            notice.len() <= notice_reserve(file_path, info),
            "{} > {}",
            notice.len(),
            notice_reserve(file_path, info)
        );
    }
}
/// Budget-starving shapes (empty or 1-byte parts, everything oversized, a 5 KB path, 64-byte names)
/// never panic and always stay within the threshold.
#[test]
fn build_truncated_never_panics_on_degenerate_inputs() {
    fn check(path: &std::path::Path, info: &ReadToolInfo, [context, query, skill]: [&str; 3]) {
        for is_cursor in [false, true] {
            let (full, layout) =
                ParsedPrompt::assemble_with_layout(context, query, skill, is_cursor);
            let bounded = build_truncated_prompt_message(
                context, query, skill, is_cursor, path, &full, &layout, info,
            );
            assert!(bounded.message.len() <= LARGE_PROMPT_THRESHOLD);
            assert!(bounded.message.contains(OFFLOAD_NOTICE_MARKER));
            for range in &bounded.elided {
                assert!(range.first_line <= range.last_line, "{range:?}");
                assert!(!range.windows.is_empty(), "{range:?}");
                assert!(range.windows.iter().all(|w| w.limit > 0), "{range:?}");
            }
            for pair in bounded.elided.windows(2) {
                let [earlier, later] = pair else {
                    unreachable!("windows(2)")
                };
                assert!(earlier.last_line < later.first_line, "disjoint");
            }
        }
    }
    let short_path = fake_prompt_path();
    let long_path = std::path::PathBuf::from(format!("/{}/prompt_0.txt", "p".repeat(5_000)));
    let defaults = ReadToolInfo::default();
    let long_names = ReadToolInfo {
        tool: Some("T".repeat(64)),
        offset: Some("O".repeat(64)),
        limit: Some("L".repeat(64)),
        max_lines: usize::MAX,
    };
    let big = "B".repeat(LARGE_PROMPT_THRESHOLD * 2);
    let parts = ["", "x", big.as_str()];
    for (path, info) in [(&short_path, &defaults), (&long_path, &long_names)] {
        for context in parts {
            for query in parts {
                for skill in parts {
                    check(path, info, [context, query, skill]);
                }
            }
        }
    }
}
/// `distill_home()` is a process-wide `OnceLock`, so the real async method is only exercised for the no-offload gate.
/// Threshold gate: a prompt exactly at `LARGE_PROMPT_THRESHOLD` is returned unchanged and no file is written.
#[tokio::test(flavor = "current_thread")]
async fn maybe_truncate_at_threshold_returns_unchanged_no_file() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 1_000_000, 85, gateway_tx, persistence_tx).await;
            let at = "Q".repeat(LARGE_PROMPT_THRESHOLD);
            let expected = crate::session::prompt_parser::ParsedPrompt::assemble_parts_with_skills(
                "", &at, "", false,
            );
            let (message, path) = actor
                .maybe_truncate_large_prompt_with_skills(
                    String::new(),
                    at,
                    String::new(),
                    false,
                    70_020,
                )
                .await;
            assert!(path.is_none(), "at-threshold prompt must not offload");
            assert_eq!(message, expected, "at-threshold prompt returned unchanged");
        })
        .await;
}
/// The finalized toolset drives the notice: without a Read tool or truncation config nothing is
/// named and the built-in cap applies; a renamed `read_file` with a seeded cap is picked up as such.
#[tokio::test(flavor = "current_thread")]
async fn resolve_read_tool_info_reads_finalized_toolset() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 1_000_000, 85, gateway_tx, persistence_tx).await;
            assert_eq!(
                ReadToolInfo::default(),
                actor.resolve_read_tool_info().await
            );
            let overrides = std::collections::HashMap::from([
                ("offset".to_owned(), "start_line".to_owned()),
                ("limit".to_owned(), "line_count".to_owned()),
            ]);
            let mut renamed = ToolConfig::from_id("Distill:read_file").with_name("open_file");
            renamed.params_name_overrides = Some(overrides);
            *actor.agent.borrow_mut() = test_agent_with_tools(vec![renamed]).await;
            let cfg = TruncationConfig {
                max_lines_read: Some(250),
                ..Default::default()
            };
            let bridge = std::sync::Arc::clone(actor.agent.borrow().tool_bridge());
            bridge
                .toolset()
                .resources
                .lock()
                .await
                .insert(TruncationCfg(cfg));
            assert_eq!(
                ReadToolInfo {
                    tool: Some("open_file".to_owned()),
                    offset: Some("start_line".to_owned()),
                    limit: Some("line_count".to_owned()),
                    max_lines: 250,
                },
                actor.resolve_read_tool_info().await
            );
        })
        .await;
}
/// Call-site wiring with the injected writer: success returns the bounded message and `Some(path)`.
/// A write failure returns the SAME bounded message and `None`, never the oversized original.
#[test]
fn write_offload_and_build_wires_offload_and_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let file_path = temp.path().join("sid").join("prompts").join("prompt_0.txt");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    let query = format!(
        "HEAD_TOKEN {} TAIL_TOKEN",
        "Q".repeat(LARGE_PROMPT_THRESHOLD * 3)
    );
    let (full, layout) = ParsedPrompt::assemble_with_layout("", &query, "", false);
    let info = ReadToolInfo::default();
    let bounded =
        build_truncated_prompt_message("", &query, "", false, &file_path, &full, &layout, &info);
    let (message, path) = write_offload_and_build(
        &full,
        bounded.clone(),
        file_path.clone(),
        crate::util::secure_file::write_secure_file,
    );
    let path = path.expect("over-threshold offload must return the file path");
    assert_eq!(path, file_path, "returned path is the offload target");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        full,
        "file holds the full message bytes"
    );
    assert_eq!(
        message, bounded.message,
        "success returns the bounded message"
    );
    assert!(!message.contains(&query), "full query body not inlined");
    assert!(
        message.contains(OFFLOAD_NOTICE_MARKER),
        "success keeps the file-referencing offload notice"
    );
    assert!(
        message.contains(&file_path.display().to_string()),
        "success points the model at the real offloaded file"
    );
    let (fallback_msg, fallback_path) =
        write_offload_and_build(&full, bounded.clone(), file_path.clone(), |_p, _b| {
            Err(std::io::Error::other("simulated disk full"))
        });
    assert!(
        fallback_path.is_none(),
        "write failure must not return an offload path"
    );
    assert_ne!(
        fallback_msg, bounded.message,
        "write failure must rewrite the notice, not return it verbatim"
    );
    assert!(
        !fallback_msg.contains(OFFLOAD_NOTICE_MARKER),
        "write failure must strip the file-referencing offload notice"
    );
    assert!(
        !fallback_msg.contains(&file_path.display().to_string()),
        "write failure must not point the model at a file that was never written"
    );
    assert!(
        fallback_msg.contains("could not be saved"),
        "write failure must explain the excerpt is all there is"
    );
    assert!(
        fallback_msg.contains("HEAD_TOKEN") && fallback_msg.contains("TAIL_TOKEN"),
        "the bounded head+tail excerpt must survive the failure path"
    );
    assert!(
        fallback_msg.len() <= LARGE_PROMPT_THRESHOLD,
        "fallback must stay within budget (no re-overflow)"
    );
    assert!(
        !fallback_msg.contains(&query),
        "fallback must not inline the full query"
    );
}
/// `strip_offload_notice` swaps the exact file-referencing notice for the failure notice that points at no file.
/// It is a no-op when the notice is absent (defensive).
#[test]
fn strip_offload_notice_swaps_notice_for_no_file_text() {
    let path = fake_prompt_path();
    let notice = build_offload_notice(45_177, 1, &path, &ReadToolInfo::default(), &[]);
    let message = format!("bounded excerpt body{notice}");
    let stripped = strip_offload_notice(&message, &notice);
    assert!(
        stripped.starts_with("bounded excerpt body"),
        "excerpt preserved"
    );
    assert!(
        !stripped.contains(OFFLOAD_NOTICE_MARKER),
        "file-referencing marker removed"
    );
    assert!(
        !stripped.contains(&path.display().to_string()),
        "file path removed"
    );
    assert!(
        stripped.contains("could not be saved"),
        "no-file failure notice substituted"
    );
    assert_eq!(
        strip_offload_notice("plain message", &notice),
        "plain message"
    );
}
/// Compat-harness ordering puts the notice mid-message, before the trailing query block.
/// A write failure must strip it in place without discarding that query block.
#[test]
fn write_offload_failure_strips_cursor_midmessage_notice() {
    let temp = tempfile::tempdir().unwrap();
    let file_path = temp.path().join("sid").join("prompts").join("prompt_0.txt");
    let query = format!(
        "QHEAD_TOKEN {} QTAIL_TOKEN",
        "Q".repeat(LARGE_PROMPT_THRESHOLD * 2)
    );
    let context = format!("CHEAD_TOKEN {}", "C".repeat(LARGE_PROMPT_THRESHOLD * 2));
    let (full, layout) = ParsedPrompt::assemble_with_layout(&context, &query, "", true);
    let info = ReadToolInfo::default();
    let bounded = build_truncated_prompt_message(
        &context, &query, "", true, &file_path, &full, &layout, &info,
    );
    assert!(bounded.message.contains(OFFLOAD_NOTICE_MARKER));
    assert!(bounded.message.ends_with("QTAIL_TOKEN"));
    let (msg, path) = write_offload_and_build(&full, bounded, file_path.clone(), |_p, _b| {
        Err(std::io::Error::other("simulated disk full"))
    });
    assert!(path.is_none(), "failed offload returns no path");
    assert!(
        !msg.contains(OFFLOAD_NOTICE_MARKER),
        "cursor mid-message notice must be stripped"
    );
    assert!(
        !msg.contains(&file_path.display().to_string()),
        "no dangling file path may leak"
    );
    assert!(
        msg.contains("could not be saved"),
        "failure notice substituted"
    );
    assert!(
        msg.ends_with("QTAIL_TOKEN"),
        "trailing query block must survive the in-place strip"
    );
    assert!(
        msg.contains("CHEAD_TOKEN"),
        "context head must survive the strip"
    );
    assert!(msg.len() <= LARGE_PROMPT_THRESHOLD);
}
