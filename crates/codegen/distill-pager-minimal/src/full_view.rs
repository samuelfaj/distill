// Modified for Distill by Samuel Fajreldines, 2026.
//! Minimal-mode "full view": the whole conversation rendered with every block fully expanded, as one ANSI string opened in `$PAGER` (`less -R`).
//! Reasoning shows in full (not the collapsed `Thought for Xs` marker) and tool output is uncapped.
//!
//! Minimal commits blocks into the terminal's *native* scrollback as static text (collapsed reasoning, truncated tool output).
//! That text cannot be re-rendered in place when the user toggles verbose.
//! So "expand everything" is served by re-rendering the whole transcript off-screen at full fidelity and handing it to a pager (transcript mode).
//! The build reuses [`EntryRenderer`] (per-block layout, syntax highlighting, diff colors) with the display mode forced to `Expanded`.
//! It then serializes the resulting cell buffer to ANSI so colors survive in the pager.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr as _;

use distill_pager::app::app_view::AppView;
use distill_pager::minimal_api;
use distill_pager::render::Renderable;
use distill_pager::scrollback::entry::ScrollbackEntry;
use distill_pager::scrollback::types::DisplayMode;
use distill_pager::scrollback::wrappers::EntryRenderer;
use distill_pager::theme::Theme;

/// Fixed render width for the transcript, independent of the current terminal size.
/// The pager wraps to the real terminal, and the committed and full-view wrapping need not match.
const FULL_VIEW_WIDTH: u16 = 100;

/// Per-frame budget for the incremental transcript build.
/// Small enough that a slice never blocks input or streaming noticeably, large enough to drain a big session in a couple of seconds.
/// Frames tick at ~16ms while a build is active (see `AppView::tick_interval_ceiling`).
const PUMP_BUDGET: Duration = Duration::from_millis(8);

/// Advance the in-progress `/transcript` build by one time-budgeted slice. It cannot move off-thread: the block
/// model is `!Send` (syntect's resumable highlighter lives inside streaming-markdown blocks).
pub fn pump_transcript(app: &mut AppView) {
    let Some(mut build) = minimal_api::take_minimal_transcript(app) else {
        return;
    };
    // Resolve against the build's OWNING agent, never the active view. `EntryId`s are per-`ScrollbackState` counters,
    // so a session switch mid-build must not re-target the snapshot at another agent's scrollback. The owner keeps
    // existing across view switches, so the build survives the user tabbing away; only a truly-removed agent drops it.
    let id = build.agent;
    let appearance = super::commit::committed_appearance(&app.appearance);
    {
        let Some(agent) = app.agents.get(&id) else {
            tracing::warn!("minimal: transcript build's agent removed; dropping the build");
            return;
        };
        let theme = Theme::current();
        let sb = &agent.scrollback;

        // Show every thinking entry THAT EXISTS in the session: this view is the advertised full-fidelity "expand
        // everything" transcript. That is a deliberate, test-encoded memory tradeoff that predates minimal. Sessions run
        // entirely with the toggle off have no thinking entries for any view to show.
        let prev_thinking = distill_pager::appearance::cache::load_show_thinking_blocks();
        distill_pager::appearance::cache::set_show_thinking_blocks(true);
        let start = Instant::now();
        while build.next < build.ids.len() {
            let Some(&eid) = build.ids.get(build.next) else {
                break;
            };
            build.next += 1;
            // Re-resolve by id: entries removed mid-build (rewind / clear) are skipped rather than skewing positions
            if let Some(entry) = sb.index_of_id(eid).and_then(|idx| sb.entry(idx)) {
                render_entry_to_ansi(
                    entry,
                    &theme,
                    &appearance,
                    &agent.session.cwd,
                    &mut build.out,
                );
            }
            if start.elapsed() >= PUMP_BUDGET {
                break;
            }
        }
        distill_pager::appearance::cache::set_show_thinking_blocks(prev_thinking);
    }

    if build.next < build.ids.len() {
        // More to do: resume next frame (ticks keep flowing via `needs_animation`; progress shows in the status row)
        minimal_api::set_minimal_transcript(app, Some(build));
        return;
    }

    // Done: hand the file to the event loop's suspend-into-$PAGER path.
    finish_transcript(app, id, build.out);
}

/// Write the finished transcript and set `pending_pager_path`; the ANSI flag makes the event loop add `-R` for `less`.
/// Errors land as a system block on the build's owning agent, which may differ from the active view (the user can tab away while the build runs).
fn finish_transcript(app: &mut AppView, id: distill_pager::app::agent::AgentId, out: String) {
    if out.is_empty() {
        if let Some(agent) = app.agents.get_mut(&id) {
            agent
                .scrollback
                .push_block(distill_pager::scrollback::block::RenderBlock::system(
                    "No conversation transcript to view yet",
                ));
        }
        return;
    }
    let path = std::env::temp_dir().join(format!("grok-transcript-{}.ansi", uuid::Uuid::new_v4()));
    match std::fs::write(&path, out) {
        Ok(()) => {
            app.pending_pager_path = Some(path);
            app.pending_pager_ansi = true;
        }
        Err(e) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.scrollback.push_block(
                    distill_pager::scrollback::block::RenderBlock::system(format!(
                        "Failed to write transcript: {e}"
                    )),
                );
            }
        }
    }
}

/// Render one entry (fully expanded, at [`FULL_VIEW_WIDTH`]) and append its ANSI serialization to `out`.
/// Cloning keeps the live entry's display mode, which drives the on-screen committed look, untouched.
fn render_entry_to_ansi(
    entry: &ScrollbackEntry,
    theme: &Theme,
    appearance: &distill_pager::appearance::AppearanceConfig,
    cwd: &std::path::Path,
    out: &mut String,
) {
    let mut expanded = entry.clone();
    expanded.set_display_mode(DisplayMode::Expanded);

    let renderer = EntryRenderer::new(&expanded, theme)
        .with_appearance(appearance.clone())
        .with_cwd(Some(cwd))
        .with_flat_background(true);
    let height = renderer.desired_height(FULL_VIEW_WIDTH);
    if height == 0 {
        return;
    }
    let area = Rect::new(0, 0, FULL_VIEW_WIDTH, height);
    let mut buf = Buffer::empty(area);
    renderer.render(area, &mut buf);
    buffer_to_ansi(&buf, out);
    // Blank line between blocks so the transcript breathes in the pager.
    out.push('\n');
}

/// Last glyph column. Trailing spaces — including painted diff/code pads — are not
/// visible for ANSI serialization, so `/transcript` and commit emit stay short in a
/// narrower pager. [`trim_trailing_pads`] still keeps those painted cells in the buffer.
pub(crate) fn last_visible_column(buf: &Buffer, y: u16) -> Option<u16> {
    let area = buf.area;
    for x in (area.x..area.x.saturating_add(area.width)).rev() {
        if let Some(cell) = buf.cell((x, y)) {
            if cell.skip {
                continue;
            }
            let s = cell.symbol();
            if !s.is_empty() && s != " " {
                return Some(x);
            }
        }
    }
    None
}

fn row_fills_width(buf: &Buffer, y: u16, last_x: u16) -> bool {
    let last_col = buf.area.x.saturating_add(buf.area.width.saturating_sub(1));
    if last_x >= last_col {
        return true;
    }
    let Some(cell) = buf.cell((last_x, y)) else {
        return false;
    };
    // Ratatui 0.29 stores a wide glyph's covered cell as `" "`, not `""`.
    // Width-aware coverage still treats CJK/emoji that end at the margin as full.
    let extra = cell.symbol().width().saturating_sub(1);
    let covered = last_x.saturating_add(u16::try_from(extra).unwrap_or(0));
    if covered < last_col {
        return false;
    }
    (last_x.saturating_add(1)..=last_col).all(|x| {
        buf.cell((x, y)).is_some_and(|c| {
            let s = c.symbol();
            s.is_empty() || s == " "
        })
    })
}

/// Skip trailing pads; leave wide-glyph continuation cells and semantic line-bg spaces.
pub(crate) fn trim_trailing_pads(buf: &mut Buffer) {
    let area = buf.area;
    for y in area.y..area.y.saturating_add(area.height) {
        let last_glyph = last_visible_column(buf, y);
        let mut start = last_glyph.map(|x| x.saturating_add(1)).unwrap_or(area.x);
        if let Some(x) = last_glyph
            && let Some(cell) = buf.cell((x, y))
        {
            let extra = cell.symbol().width().saturating_sub(1);
            if extra > 0 {
                start = start.saturating_add(u16::try_from(extra).unwrap_or(0));
            }
        }
        for x in start..area.x.saturating_add(area.width) {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            if cell.symbol().is_empty() {
                continue;
            }
            // Diff insert/delete bands live on these spaces; resetting them drops the color from committed ANSI.
            if cell.bg != Color::Reset {
                continue;
            }
            cell.reset();
            cell.skip = true;
        }
    }
}

/// Visit painted glyphs through `last_x`, skipping wide-glyph continuation cells
/// (`""` or Ratatui 0.29 `" "`).
fn for_each_row_glyph(
    buf: &Buffer,
    y: u16,
    last_x: u16,
    mut visit: impl FnMut(&str, Color, Color, Modifier),
) {
    let mut skip = 0usize;
    for x in buf.area.x..=last_x {
        let Some(cell) = buf.cell((x, y)) else {
            continue;
        };
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let sym = cell.symbol();
        if sym.is_empty() {
            continue;
        }
        skip = sym.width().saturating_sub(1);
        visit(sym, cell.fg, cell.bg, cell.modifier);
    }
}

/// Write styled glyphs through `last_x` with no trailing newline or reset.
fn write_row_ansi(buf: &Buffer, y: u16, last_x: u16, out: &mut String) {
    // Track the current style as the raw (fg, bg, modifier) tuple and only build the escape string on a run boundary
    // Comparing three Copy fields per cell is far cheaper than building and comparing an SGR string per cell
    // The per-cell string build was the hot spot on long transcripts
    let mut cur: Option<(Color, Color, Modifier)> = None;
    let mut sgr = String::with_capacity(32);
    for_each_row_glyph(buf, y, last_x, |sym, fg, bg, modifier| {
        let style = (fg, bg, modifier);
        if cur != Some(style) {
            cell_sgr(style.0, style.1, style.2, &mut sgr);
            out.push_str(&sgr);
            cur = Some(style);
        }
        out.push_str(sym);
    });
}

/// Serialize a rendered cell [`Buffer`] to ANSI text (one `\n`-terminated line per row).
/// An SGR sequence is emitted whenever the style changes, with a reset at each row end.
/// Trailing blank cells are trimmed so lines stay short.
fn buffer_to_ansi(buf: &Buffer, out: &mut String) {
    let area = buf.area;
    for y in area.y..area.y.saturating_add(area.height) {
        if let Some(last_x) = last_visible_column(buf, y) {
            write_row_ansi(buf, y, last_x, out);
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
}

/// `(ansi without trailing newline, fills the last column, soft-wraps onto the next row)`.
/// `wraps` is the renderer joiner / WRAPLINE signal, not "row is full-width".
pub(crate) type SemanticRow = (String, bool, bool);

fn row_wraps(wraps: &[bool], area_y: u16, y: u16) -> bool {
    y.checked_sub(area_y)
        .and_then(|i| wraps.get(usize::from(i)).copied())
        .unwrap_or(false)
}

/// Trim pads. Soft-wrap only when `wraps` says the next row is a continuation.
pub(crate) fn buffer_to_semantic_rows(buf: &Buffer, wraps: &[bool]) -> Vec<SemanticRow> {
    let area = buf.area;
    let mut rows = Vec::with_capacity(area.height as usize);
    for y in area.y..area.y.saturating_add(area.height) {
        match last_visible_column(buf, y) {
            Some(last_x) => {
                let mut ansi = String::new();
                write_row_ansi(buf, y, last_x, &mut ansi);
                let fills = row_fills_width(buf, y, last_x);
                let next_y = y.saturating_add(1);
                let next_has = next_y < area.y.saturating_add(area.height)
                    && last_visible_column(buf, next_y).is_some();
                let wraps_row = row_wraps(wraps, area.y, y);
                // Omit the trailing reset only when xenl will consume: CSI would clear wrap-pending.
                // A short joiner (wraps, not full-width) still hard-breaks.
                if !(wraps_row && fills && next_has) {
                    ansi.push_str("\x1b[0m");
                }
                rows.push((ansi, fills, wraps_row));
            }
            None => rows.push((String::new(), false, false)),
        }
    }
    rows
}

#[cfg(test)]
pub(crate) fn buffer_to_semantic_copy(buf: &Buffer, wraps: &[bool]) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y.saturating_add(area.height) {
        match last_visible_column(buf, y) {
            Some(last_x) => {
                for_each_row_glyph(buf, y, last_x, |s, _, _, _| out.push_str(s));
                let next_y = y.saturating_add(1);
                let next_empty = next_y >= area.y.saturating_add(area.height)
                    || last_visible_column(buf, next_y).is_none();
                if !row_wraps(wraps, area.y, y) || next_empty {
                    out.push('\n');
                }
            }
            None => out.push('\n'),
        }
    }
    out
}

/// Build a full SGR sequence (leading reset, then modifiers, fg, bg) for a cell's style. The caller emits it only
/// when the style changes, so the reset can't leak attributes across cells. Writes into `sgr` (cleared first)
/// instead of allocating.
fn cell_sgr(fg: Color, bg: Color, modifier: Modifier, sgr: &mut String) {
    use std::fmt::Write as _;

    sgr.clear();
    sgr.push_str("\x1b[0");
    if modifier.contains(Modifier::BOLD) {
        sgr.push_str(";1");
    }
    if modifier.contains(Modifier::DIM) {
        sgr.push_str(";2");
    }
    if modifier.contains(Modifier::ITALIC) {
        sgr.push_str(";3");
    }
    if modifier.contains(Modifier::UNDERLINED) {
        sgr.push_str(";4");
    }
    if modifier.contains(Modifier::REVERSED) {
        sgr.push_str(";7");
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        sgr.push_str(";9");
    }
    sgr.push(';');
    let _ = write!(sgr, "{}", color_code(fg, false));
    sgr.push(';');
    let _ = write!(sgr, "{}", color_code(bg, true));
    sgr.push('m');
}

/// Map a ratatui [`Color`] to its SGR parameter (foreground, or background when `bg`).
/// Named colors use the 16-color codes; `Indexed`/`Rgb` use the 256 / truecolor forms.
/// `Reset` is the terminal default (39 fg / 49 bg).
fn color_code(color: Color, bg: bool) -> String {
    // Named-color base code (30-series fg); +10 shifts to the 40-series bg.
    let named = |n: u16| -> String { (if bg { n + 10 } else { n }).to_string() };
    match color {
        Color::Reset => named(39),
        Color::Black => named(30),
        Color::Red => named(31),
        Color::Green => named(32),
        Color::Yellow => named(33),
        Color::Blue => named(34),
        Color::Magenta => named(35),
        Color::Cyan => named(36),
        Color::Gray => named(37),
        Color::DarkGray => named(90),
        Color::LightRed => named(91),
        Color::LightGreen => named(92),
        Color::LightYellow => named(93),
        Color::LightBlue => named(94),
        Color::LightMagenta => named(95),
        Color::LightCyan => named(96),
        Color::White => named(97),
        Color::Indexed(i) => format!("{};5;{}", if bg { 48 } else { 38 }, i),
        Color::Rgb(r, g, b) => format!("{};2;{};{};{}", if bg { 48 } else { 38 }, r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_pager::scrollback::block::RenderBlock;

    fn test_cwd() -> &'static std::path::Path {
        std::path::Path::new("/test/session")
    }

    /// With the `[ui]` show_thinking_blocks toggle off, a thinking entry renders zero rows and vanished from the "full-fidelity" transcript.
    /// The pump enables the (thread-local) toggle for the build.
    /// This locks the mechanism: off omits the entry, on (what `pump_transcript` sets) includes it.
    #[test]
    fn transcript_includes_thinking_when_pump_enables_toggle() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &distill_pager::appearance::AppearanceConfig::default(),
        );
        let entry = ScrollbackEntry::new(RenderBlock::thinking(
            "deep reasoning about haikus and syllables",
        ));

        distill_pager::appearance::cache::set_show_thinking_blocks(false);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        assert!(
            out.is_empty(),
            "thinking hidden while the toggle is off: {out:?}"
        );

        // What `pump_transcript` sets for the duration of a slice.
        distill_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        distill_pager::appearance::cache::set_show_thinking_blocks(false);
        assert!(
            out.contains("reasoning"),
            "thinking content included in the transcript: {out:?}"
        );
    }

    /// A thinking entry built the way live streaming builds it (streaming block, per-chunk pushes, finish) must render its BODY in the transcript.
    /// The collapsed "Thought for Xs" header alone is not enough.
    #[test]
    fn transcript_expands_streamed_thinking_body() {
        use distill_pager::scrollback::state::ScrollbackState;

        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &distill_pager::appearance::AppearanceConfig::default(),
        );

        distill_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let id = sb.push_block(RenderBlock::thinking_streaming());
        assert!(sb.push_chunk_to_thinking(id, "REASONINGBODY pondering "));
        sb.push_chunk_to_thinking(id, "quietly about wraps");
        sb.finish_running_with_time(id, Some(1200));

        let entry = sb.get_by_id(id).expect("thinking entry");
        let mut out = String::new();
        render_entry_to_ansi(entry, &theme, &appearance, test_cwd(), &mut out);
        distill_pager::appearance::cache::set_show_thinking_blocks(false);

        assert!(
            out.contains("REASONINGBODY"),
            "transcript must include the streamed thinking body: {out:?}"
        );
    }

    /// `minimal_collapse_thinking` points users at `/transcript` for the full text, so the transcript must ignore the committed display mode.
    #[test]
    fn transcript_expands_thinking_committed_collapsed() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &distill_pager::appearance::AppearanceConfig {
                minimal_collapse_thinking: true,
                ..Default::default()
            },
        );
        let mut entry = ScrollbackEntry::new(RenderBlock::thinking(
            "REASONINGBODY folded away at commit time",
        ));
        entry.set_display_mode(super::super::commit::minimal_commit_display_mode(
            &entry.block,
            &appearance,
        ));
        assert_eq!(entry.display_mode(), DisplayMode::Collapsed);

        distill_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        distill_pager::appearance::cache::set_show_thinking_blocks(false);

        assert!(
            out.contains("REASONINGBODY"),
            "a collapsed commit must still expand in /transcript: {out:?}"
        );
        assert!(
            !out.contains("ctrl+e to expand"),
            "no expand hint in the fully-expanded transcript: {out:?}"
        );
    }

    #[test]
    fn transcript_uses_owning_session_cwd_for_tool_paths() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &distill_pager::appearance::AppearanceConfig::default(),
        );
        let entry =
            ScrollbackEntry::new(RenderBlock::edit("/alternate/worktree/src/main.rs", None));
        let mut out = String::new();

        render_entry_to_ansi(
            &entry,
            &theme,
            &appearance,
            std::path::Path::new("/alternate/worktree"),
            &mut out,
        );

        assert!(out.contains("src/main.rs"), "transcript: {out:?}");
        assert!(
            !out.contains("/alternate/worktree"),
            "session prefix should be elided: {out:?}"
        );
    }

    #[test]
    fn color_code_maps_reset_named_indexed_rgb() {
        assert_eq!(color_code(Color::Reset, false), "39");
        assert_eq!(color_code(Color::Reset, true), "49");
        assert_eq!(color_code(Color::Red, false), "31");
        assert_eq!(color_code(Color::Red, true), "41");
        assert_eq!(color_code(Color::DarkGray, false), "90");
        assert_eq!(color_code(Color::DarkGray, true), "100");
        assert_eq!(color_code(Color::Indexed(200), false), "38;5;200");
        assert_eq!(color_code(Color::Rgb(1, 2, 3), true), "48;2;1;2;3");
    }

    #[test]
    fn cell_sgr_includes_modifiers_and_colors() {
        let mut sgr = String::new();
        cell_sgr(
            Color::Rgb(10, 20, 30),
            Color::Reset,
            Modifier::BOLD | Modifier::ITALIC,
            &mut sgr,
        );
        // Leading reset, bold, italic, truecolor fg, default bg.
        assert_eq!(sgr, "\x1b[0;1;3;38;2;10;20;30;49m");
        // Reused buffer is cleared, not appended.
        cell_sgr(Color::Red, Color::Reset, Modifier::empty(), &mut sgr);
        assert_eq!(sgr, "\x1b[0;31;49m");
    }

    fn strip_sgr(ansi: &str) -> String {
        let mut out = String::new();
        let mut chars = ansi.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn buffer_to_ansi_trims_trailing_and_terminates_rows() {
        // A 6-wide, 2-row buffer: "hi" on row 0, blank row 1.
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 2));
        buf.cell_mut((0, 0)).unwrap().set_symbol("h");
        buf.cell_mut((1, 0)).unwrap().set_symbol("i");
        let mut out = String::new();
        buffer_to_ansi(&buf, &mut out);
        let lines: Vec<&str> = out.split('\n').collect();
        // Row 0 has content ending in a reset; row 1 is blank; trailing newline.
        let Some(row0) = lines.first() else {
            panic!("expected a rendered row: {lines:?}");
        };
        assert!(row0.contains('h') && row0.contains('i'));
        assert!(row0.ends_with("\x1b[0m"), "row must reset: {row0:?}");
        assert_eq!(
            lines.get(1).copied(),
            Some(""),
            "blank row emits nothing but the newline"
        );
        assert!(
            !row0.contains("  "),
            "trailing spaces not trimmed: {row0:?}"
        );
    }

    /// Painted insert/delete pads stay in the buffer, but transcript/ANSI/copy stop at the last glyph
    /// so a 100-col render does not reflow in an 80-col `$PAGER`.
    #[test]
    fn transcript_ansi_does_not_reflow_from_painted_pad_spaces() {
        let band = Color::Rgb(0, 40, 0);
        let width = FULL_VIEW_WIDTH;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        for x in 0..width {
            let Some(cell) = buf.cell_mut((x, 0)) else {
                continue;
            };
            cell.set_bg(band);
            if x == 0 {
                cell.set_symbol("h");
            } else if x == 1 {
                cell.set_symbol("i");
            } else {
                cell.set_symbol(" ");
            }
        }
        trim_trailing_pads(&mut buf);

        let last_pad = buf.cell((width.saturating_sub(1), 0)).expect("last pad");
        assert_eq!(last_pad.bg, band, "commit paint keeps the diff-band pad");
        assert!(!last_pad.skip, "painted pad is not a skip cell");
        assert_eq!(
            last_visible_column(&buf, 0),
            Some(1),
            "serialization visibility stops at the last glyph"
        );

        let mut transcript = String::new();
        buffer_to_ansi(&buf, &mut transcript);
        let visible = strip_sgr(&transcript);
        assert_eq!(
            visible, "hi\n",
            "transcript must not emit a full-width space band: {transcript:?}"
        );
        assert!(
            visible.trim_end_matches('\n').chars().count() < 80,
            "an 80-col pager must not reflow this row: {visible:?}"
        );
        assert!(
            transcript.contains("48;2;0;40;0"),
            "glyph cells still carry the insert-band SGR: {transcript:?}"
        );

        let copy = buffer_to_semantic_copy(&buf, &[false]);
        assert_eq!(copy, "hi\n", "native copy must not include painted pads");

        let rows = buffer_to_semantic_rows(&buf, &[false]);
        let Some((ansi, fills, _)) = rows.first() else {
            panic!("expected a serialized row");
        };
        assert_eq!(strip_sgr(ansi), "hi");
        assert!(
            !fills,
            "short painted-pad row must not fill the width: {ansi:?}"
        );
        assert!(
            ansi.contains("48;2;0;40;0"),
            "committed ANSI keeps the insert-band SGR on glyphs: {ansi:?}"
        );
    }

    #[test]
    fn semantic_copy_full_width_before_blank_stays_a_paragraph() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        buf.cell_mut((0, 0)).unwrap().set_symbol("a");
        buf.cell_mut((1, 0)).unwrap().set_symbol("b");
        buf.cell_mut((2, 0)).unwrap().set_symbol("c");
        buf.cell_mut((3, 0)).unwrap().set_symbol("d");
        buf.cell_mut((0, 2)).unwrap().set_symbol("x");
        let copy = buffer_to_semantic_copy(&buf, &[]);
        assert_eq!(copy, "abcd\n\nx\n");
    }

    #[test]
    fn semantic_copy_exact_width_hard_break_does_not_join() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf.cell_mut((0, 0)).unwrap().set_symbol("a");
        buf.cell_mut((1, 0)).unwrap().set_symbol("b");
        buf.cell_mut((2, 0)).unwrap().set_symbol("c");
        buf.cell_mut((3, 0)).unwrap().set_symbol("d");
        buf.cell_mut((0, 1)).unwrap().set_symbol("x");
        buf.cell_mut((1, 1)).unwrap().set_symbol("y");
        let copy = buffer_to_semantic_copy(&buf, &[]);
        assert_eq!(copy, "abcd\nxy\n");
        assert!(
            !copy.contains("abcdxy"),
            "exact-width hard break must not join: {copy:?}"
        );
    }

    #[test]
    fn semantic_copy_joins_soft_wrapped_rows() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf.cell_mut((0, 0)).unwrap().set_symbol("a");
        buf.cell_mut((1, 0)).unwrap().set_symbol("b");
        buf.cell_mut((2, 0)).unwrap().set_symbol("c");
        buf.cell_mut((3, 0)).unwrap().set_symbol("d");
        buf.cell_mut((0, 1)).unwrap().set_symbol("e");
        let copy = buffer_to_semantic_copy(&buf, &[true, false]);
        assert_eq!(copy, "abcde\n");

        let mut transcript = String::new();
        buffer_to_ansi(&buf, &mut transcript);
        assert!(
            transcript.contains('\n'),
            "transcript still terminates every visual row"
        );
        let visual_rows: Vec<&str> = transcript.split('\n').collect();
        assert!(
            visual_rows.len() >= 2,
            "transcript keeps one newline per buffer row: {transcript:?}"
        );
    }

    #[test]
    fn trim_preserves_wide_glyph_wrap() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf.cell_mut((0, 0)).unwrap().set_symbol("a");
        buf.cell_mut((1, 0)).unwrap().set_symbol("b");
        buf.cell_mut((2, 0)).unwrap().set_symbol("中");
        buf.cell_mut((3, 0)).unwrap().set_symbol("");
        buf.cell_mut((0, 1)).unwrap().set_symbol("x");
        buf.cell_mut((1, 1)).unwrap().set_symbol("y");
        trim_trailing_pads(&mut buf);

        let cont = buf.cell((3, 0)).expect("continuation");
        assert!(
            cont.symbol().is_empty(),
            "wide-glyph spacer must survive trim"
        );
        assert!(!cont.skip, "continuation is not a pad");

        let wraps = [true, false];
        let rows = buffer_to_semantic_rows(&buf, &wraps);
        let first = rows.first().expect("row 0");
        assert!(first.1, "CJK wrap row must still fill the width");
        assert!(first.2, "CJK wrap row carries the joiner signal");
        let copy = buffer_to_semantic_copy(&buf, &wraps);
        assert!(
            copy.starts_with("ab中xy"),
            "wide-glyph wrap must join, not hard-break: {copy:?}"
        );
    }

    /// Ratatui 0.29 stores the covered cell as `" "`. That must still fill the row.
    #[test]
    fn ratatui_wide_glyph_space_continuation_fills_width() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf.set_string(0, 0, "ab中", ratatui::style::Style::default());
        buf.set_string(0, 1, "xy", ratatui::style::Style::default());
        let cont = buf.cell((3, 0)).expect("continuation");
        assert_eq!(
            cont.symbol(),
            " ",
            "precondition: ratatui 0.29 covers a wide glyph with a space cell"
        );

        trim_trailing_pads(&mut buf);
        let cont = buf.cell((3, 0)).expect("continuation after trim");
        assert!(!cont.skip, "space continuation must survive trim: {cont:?}");
        assert_eq!(cont.symbol(), " ", "continuation stays a space cell");

        let wraps = [true, false];
        let rows = buffer_to_semantic_rows(&buf, &wraps);
        let first = rows.first().expect("row 0");
        assert!(
            first.1,
            "CJK/emoji row ending at the margin must fill_width: {rows:?}"
        );
        let copy = buffer_to_semantic_copy(&buf, &wraps);
        assert!(
            copy.starts_with("ab中xy"),
            "wide-glyph wrap must join, not hard-break: {copy:?}"
        );
    }

    #[test]
    fn mid_row_wide_glyph_space_continuation_is_not_emitted() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 1));
        buf.set_string(0, 0, "中xy", ratatui::style::Style::default());
        assert_eq!(
            buf.cell((1, 0)).map(|c| c.symbol().to_string()).as_deref(),
            Some(" "),
            "precondition: covered cell is a space"
        );
        let mut transcript = String::new();
        buffer_to_ansi(&buf, &mut transcript);
        assert_eq!(
            strip_sgr(&transcript),
            "中xy\n",
            "continuation space must not serialize: {transcript:?}"
        );
        let copy = buffer_to_semantic_copy(&buf, &[false]);
        assert_eq!(
            copy, "中xy\n",
            "native copy must not keep a spacer: {copy:?}"
        );
    }
}
