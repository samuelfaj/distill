//! Helper that mines the "next concrete step" inlined into the goal continuation nudge from the planner-emitted plan file.
//!
//! Verifier-verdict gaps are delivered on a separate, more prominent path, so this module only reads the plan's next unchecked item.
//! That path is `render_verifier_gaps_block` in `acp_session.rs`, fed from the persisted `last_classifier_gaps` summary.
//! If the plan read fails, the caller falls back to a generic "check your todo list" line, so the helper here never returns it itself.

use std::path::Path;

/// Each goal nudge fires per turn, so a hostile or runaway verdict/plan must not blow up the context.
pub(crate) const MAX_READ_BYTES: usize = 8 * 1024;

/// Returns `None` on any I/O failure (missing file, permission denied, etc.).
/// When the buffer reaches the cap, the trailing potentially-incomplete line is dropped.
/// That keeps a bullet that spans the cap boundary from leaking a half-truncated tail upstream.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(MAX_READ_BYTES.min(4096));
    file.take(MAX_READ_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() >= MAX_READ_BYTES
        && let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n')
    {
        buf.truncate(last_nl);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// First unchecked `- [ ]` (or `* [ ]` / `+ [ ]`) markdown checkbox in a plan file; `- [x]` / `- [X]` are skipped, `None` when none remain.
/// Numbered `## Acceptance criteria` are not mined: they never get checked off, so criterion 1 would surface forever.
/// When the plan has a `## Task checklist` section only its checkboxes are mined.
pub(crate) fn first_unchecked_plan_item(path: &Path) -> Option<String> {
    let body = read_capped(path)?;
    extract_first_unchecked(&body)
}

/// Case-insensitive match of a markdown header line (`#`-prefixed at any level) against a section `name` ("task checklist", "non-goals", ...).
fn is_section_header(line: &str, name: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return false;
    }
    let title = trimmed.trim_start_matches('#').trim();
    title.eq_ignore_ascii_case(name)
}

fn is_any_header(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn header_level(line: &str) -> usize {
    line.trim_start().chars().take_while(|c| *c == '#').count()
}

/// Deeper subheaders (e.g. `### Phase 1`) stay inside the section; only a header at the checklist's own level or shallower ends it.
fn first_unchecked_in_checklist(body: &str) -> Option<String> {
    let mut section_level: Option<usize> = None;
    for line in body.lines() {
        if is_section_header(line, "task checklist") {
            section_level = Some(header_level(line));
            continue;
        }
        let Some(level) = section_level else {
            continue;
        };
        if is_any_header(line) && header_level(line) <= level {
            return None; // The section ended before any unchecked item was found
        }
        if let Some(item) = parse_checkbox_item(line.trim_start()) {
            return Some(item);
        }
    }
    None
}

/// Sections whose checkboxes must never be mined as a next step.
const EXCLUDED_SECTIONS: &[&str] = &["non-goals", "deviations"];

fn extract_first_unchecked(body: &str) -> Option<String> {
    let has_checklist = body.lines().any(|l| is_section_header(l, "task checklist"));
    if has_checklist {
        return first_unchecked_in_checklist(body);
    }
    let mut excluded = false;
    for line in body.lines() {
        if is_any_header(line) {
            excluded = EXCLUDED_SECTIONS
                .iter()
                .any(|name| is_section_header(line, name));
            continue;
        }
        if excluded {
            continue;
        }
        if let Some(item) = parse_checkbox_item(line.trim_start()) {
            return Some(item);
        }
    }
    None
}

fn strip_bullet_marker(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .map(str::trim_start)
}

/// STRICTLY requires the literal `[ ]` glyph after the bullet marker; a plain `- foo` bullet returns `None`.
/// `- [x]` / `- [X]` (resolved) also return `None` so iteration continues to the next unchecked item.
fn parse_checkbox_item(trimmed: &str) -> Option<String> {
    let after_marker = strip_bullet_marker(trimmed)?;
    let after_checkbox = after_marker.strip_prefix("[ ]")?;
    let text = after_checkbox.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn write_temp(body: &str) -> NamedTempFile {
        let f = NamedTempFile::new().expect("temp file must be creatable");
        std::fs::write(f.path(), body).expect("write must succeed");
        f
    }

    #[test]
    fn plan_extracts_first_unchecked_item() {
        let body = "# Plan\n\n- [x] done\n- [x] also done\n- [ ] write the integration test\n- [ ] ship it\n";
        assert_eq!(
            extract_first_unchecked(body).as_deref(),
            Some("write the integration test"),
        );
    }

    #[test]
    fn plan_tolerates_indented_and_alt_bullets() {
        let body = "  - [x] one\n   * [ ] indented star item\n";
        assert_eq!(
            extract_first_unchecked(body).as_deref(),
            Some("indented star item"),
        );
    }

    #[test]
    fn plan_returns_none_when_all_items_checked() {
        let body = "- [x] one\n- [x] two\n";
        assert!(extract_first_unchecked(body).is_none());
    }

    #[test]
    fn plan_returns_none_on_empty_unchecked_label() {
        let body = "- [ ]   \n- [ ] real item\n";
        assert_eq!(extract_first_unchecked(body).as_deref(), Some("real item"),);
    }

    #[test]
    fn plan_does_not_mine_numbered_acceptance_criteria() {
        let numbered = "# Plan\n\n## Acceptance criteria\n\n1. app is created\n2. physics works\n";
        assert!(extract_first_unchecked(numbered).is_none());
        // Even with an inline `[ ]` glyph, a numbered item lacks the bullet marker `parse_checkbox_item` requires, so it is ignored
        let numbered_checkbox = "1. [ ] still a criterion, not a checkbox\n";
        assert!(extract_first_unchecked(numbered_checkbox).is_none());
    }

    #[test]
    fn plan_plain_bullet_without_checkbox_returns_none() {
        let body = "## Non-goals\n- a plain bullet\n- another plain bullet\n";
        assert!(extract_first_unchecked(body).is_none());
    }

    #[test]
    fn plan_checkbox_edge_cases() {
        let no_space = "- [ ]foo no space\n";
        assert_eq!(
            extract_first_unchecked(no_space).as_deref(),
            Some("foo no space"),
        );

        let upper_x = "- [X] uppercase done\n- [ ] real\n";
        assert_eq!(extract_first_unchecked(upper_x).as_deref(), Some("real"),);
    }

    #[test]
    fn checklist_section_scopes_extraction() {
        let body = "# Plan\n\n## Task checklist\n- [x] scaffold\n- [ ] wire input handling\n\n## Notes\n- [ ] stray box elsewhere\n";
        assert_eq!(
            extract_first_unchecked(body).as_deref(),
            Some("wire input handling"),
        );
        // All checklist items are done, so the result is None even though a stray box exists
        let done = "## Task checklist\n- [x] scaffold\n\n## Notes\n- [ ] stray box\n";
        assert!(extract_first_unchecked(done).is_none());
    }

    #[test]
    fn checklist_subheaders_do_not_end_the_section() {
        let body = "## Task checklist\n### Phase 1\n- [x] done\n### Phase 2\n- [ ] phase two step\n\n## Notes\n- [ ] stray box\n";
        assert_eq!(
            extract_first_unchecked(body).as_deref(),
            Some("phase two step"),
        );
        let ended = "## Task checklist\n- [x] done\n## Notes\n- [ ] stray box\n";
        assert!(extract_first_unchecked(ended).is_none());
    }

    #[test]
    fn non_goals_and_deviations_checkboxes_are_excluded() {
        let body =
            "## Non-goals\n- [ ] out-of-scope feature\n\n## Deviations\n- [ ] noted deviation\n";
        assert!(extract_first_unchecked(body).is_none());
        // A real checkbox after an excluded section is still found.
        let mixed = "## Non-goals\n- [ ] out of scope\n\n## Steps\n- [ ] real next step\n";
        assert_eq!(
            extract_first_unchecked(mixed).as_deref(),
            Some("real next step"),
        );
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::path::PathBuf::from("/definitely/not/a/real/path/xyzzy.md");
        assert!(first_unchecked_plan_item(&path).is_none());
    }

    #[test]
    fn malformed_markdown_returns_none_without_panicking() {
        let f = write_temp("# Plan\n\n");
        assert!(first_unchecked_plan_item(f.path()).is_none());
    }

    /// Invalid UTF-8 must not panic; `from_utf8_lossy` substitutes `U+FFFD` and the extractor returns the item after the garbage.
    #[test]
    fn invalid_utf8_plan_body_is_tolerated() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b"\xFF\xFE garbage\n- [ ] real step\n".as_slice()).unwrap();
        assert_eq!(
            first_unchecked_plan_item(f.path()).as_deref(),
            Some("real step"),
        );
    }

    #[test]
    fn read_is_capped_at_max_read_bytes() {
        assert_eq!(
            MAX_READ_BYTES,
            8 * 1024,
            "documented 8 KiB cap must not drift",
        );

        let mut body = String::new();
        body.push_str("- [ ] visible step\n");
        // Pad past the cap; lines after this point must stay invisible.
        while body.len() < MAX_READ_BYTES + 32 {
            body.push('x');
        }
        body.push_str("\n- [ ] HIDDEN past cap\n");
        assert!(body.len() > MAX_READ_BYTES + 16);

        let f = write_temp(&body);

        let raw = read_capped(f.path()).expect("read must succeed");
        assert!(
            !raw.contains("HIDDEN"),
            "read_capped must drop content past the cap: {raw}",
        );
        assert!(raw.contains("visible step"));

        let item = first_unchecked_plan_item(f.path()).expect("must extract within cap");
        assert!(
            !item.contains("HIDDEN"),
            "extractor must hide content past MAX_READ_BYTES: {item}",
        );
        assert!(item.contains("visible step"));
    }

    #[test]
    fn bullet_spanning_cap_boundary_is_dropped() {
        let mut body = String::new();
        body.push_str("- [ ] short visible step\n");
        // Pad just short of the cap, then plant an item that crosses it.
        while body.len() < MAX_READ_BYTES - 32 {
            body.push('x');
        }
        body.push_str("\n- [ ] ");
        let cross_text = "y".repeat(128);
        body.push_str(&cross_text);
        body.push('\n');
        assert!(body.len() > MAX_READ_BYTES);

        let f = write_temp(&body);
        let item = first_unchecked_plan_item(f.path()).expect("must extract within cap");
        assert_eq!(
            item, "short visible step",
            "must return the in-cap item; truncated tail must not leak: {item}",
        );
    }

    #[test]
    fn read_capped_handles_files_smaller_than_cap() {
        let f = write_temp("- [ ] tiny step\n");
        assert_eq!(
            first_unchecked_plan_item(f.path()).as_deref(),
            Some("tiny step"),
        );
    }
}
