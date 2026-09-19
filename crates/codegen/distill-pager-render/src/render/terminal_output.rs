//! Bash/terminal tool output arrives as a raw PTY byte stream.
//! It can contain ANSI SGR (colors/styles), cursor movement, line erases, and carriage returns (progress bars rewriting a line).
//! ratatui paints text verbatim and does not interpret these, so without this module the scrollback shows literal escape codes like `[1m[36m`.
//!
//! [`render_terminal_lines`] feeds the stream through a minimal, line-oriented VTE emulator (built on the `vte` parser).
//! It produces styled [`Line`]s plus de-escaped plain text: what a terminal would actually display.
//! Unlike a screen/grid emulator it keeps an unbounded, fully-styled transcript that maps onto the pager's line model.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use vte::{Params, Parser, Perform};

use crate::theme::color_support::quantize;

/// Bound transcript growth against pathological cursor jumps.
/// Tool output is already truncated upstream; these only guard against escape-code abuse.
const MAX_ROWS: usize = 50_000;
const MAX_COLS: usize = 8_192;

/// A single rendered transcript line: styled spans plus de-escaped plain text.
pub struct RenderedLine {
    pub line: Line<'static>,
    pub plain: String,
}

/// Fresh emulator per call, so render and height-cache paths stay deterministic and idempotent.
pub fn render_terminal_lines(raw: &str, base: Style) -> Vec<RenderedLine> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut sink = TermSink::new(base);
    let mut parser = Parser::new();
    parser.advance(&mut sink, raw.as_bytes());
    sink.finish()
}

/// De-escaped, cursor-resolved plain text of a terminal stream, for clipboard/search.
/// Lines are joined with `\n`.
pub fn render_terminal_plain(raw: &str) -> String {
    render_terminal_lines(raw, Style::default())
        .into_iter()
        .map(|rl| rl.plain)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    style: Style,
}

struct TermSink {
    base: Style,
    cur: Style,
    rows: Vec<Vec<Cell>>,
    row: usize,
    col: usize,
}

impl TermSink {
    fn new(base: Style) -> Self {
        Self {
            base,
            cur: base,
            rows: vec![Vec::new()],
            row: 0,
            col: 0,
        }
    }

    fn ensure_row(&mut self) {
        if self.row >= MAX_ROWS {
            self.row = MAX_ROWS - 1;
        }
        while self.rows.len() <= self.row {
            self.rows.push(Vec::new());
        }
    }

    fn put(&mut self, ch: char) {
        if self.col >= MAX_COLS {
            return;
        }
        self.ensure_row();
        let blank = Cell {
            ch: ' ',
            style: self.base,
        };
        let Some(line) = self.rows.get_mut(self.row) else {
            return;
        };
        if self.col >= line.len() {
            line.resize(self.col + 1, blank);
        }
        if let Some(slot) = line.get_mut(self.col) {
            *slot = Cell {
                ch,
                style: self.cur,
            };
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        self.ensure_row();
    }

    fn erase_line(&mut self, mode: u16) {
        self.ensure_row();
        let blank = Cell {
            ch: ' ',
            style: self.base,
        };
        let Some(line) = self.rows.get_mut(self.row) else {
            return;
        };
        match mode {
            0 => line.truncate(self.col.min(line.len())),
            1 => {
                let end = (self.col + 1).min(line.len());
                if let Some(prefix) = line.get_mut(..end) {
                    prefix.fill(blank);
                }
            }
            2 => line.clear(),
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.ensure_row();
                let Some(line) = self.rows.get_mut(self.row) else {
                    return;
                };
                let len = line.len();
                line.truncate(self.col.min(len));
                self.rows.truncate(self.row + 1);
            }
            2 | 3 => {
                self.rows.clear();
                self.rows.push(Vec::new());
                self.row = 0;
                self.col = 0;
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.cur = self.base;
            return;
        }
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < groups.len() {
            let code = groups.get(i).and_then(|g| g.first()).copied().unwrap_or(0);
            match code {
                0 => self.cur = self.base,
                1 => self.cur = self.cur.add_modifier(Modifier::BOLD),
                2 => self.cur = self.cur.add_modifier(Modifier::DIM),
                3 => self.cur = self.cur.add_modifier(Modifier::ITALIC),
                4 => self.cur = self.cur.add_modifier(Modifier::UNDERLINED),
                7 => self.cur = self.cur.add_modifier(Modifier::REVERSED),
                22 => self.cur = self.cur.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => self.cur = self.cur.remove_modifier(Modifier::ITALIC),
                24 => self.cur = self.cur.remove_modifier(Modifier::UNDERLINED),
                27 => self.cur = self.cur.remove_modifier(Modifier::REVERSED),
                30..=37 => self.cur.fg = Some(quantize(ansi16(code - 30))),
                39 => self.cur.fg = self.base.fg,
                40..=47 => self.cur.bg = Some(quantize(ansi16(code - 40))),
                49 => self.cur.bg = self.base.bg,
                90..=97 => self.cur.fg = Some(quantize(ansi16_bright(code - 90))),
                100..=107 => self.cur.bg = Some(quantize(ansi16_bright(code - 100))),
                38 => {
                    if let Some(c) = ext_color(&groups, &mut i) {
                        self.cur.fg = Some(quantize(c));
                    }
                }
                48 => {
                    if let Some(c) = ext_color(&groups, &mut i) {
                        self.cur.bg = Some(quantize(c));
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn finish(mut self) -> Vec<RenderedLine> {
        // `str::lines()` ignores a single trailing newline; mirror that so a command ending in `\n` does not gain a spurious blank line
        if self.rows.last().is_some_and(|r| r.is_empty()) {
            self.rows.pop();
        }
        let base = self.base;
        self.rows
            .into_iter()
            .map(|cells| row_to_line(cells, base))
            .collect()
    }
}

impl Perform for TermSink {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => self.col = (self.col / 8 + 1) * 8,
            0x08 => self.col = self.col.saturating_sub(1),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            'm' => self.apply_sgr(params),
            'K' => self.erase_line(first_param(params, 0)),
            'J' => self.erase_display(first_param(params, 0)),
            'A' => self.row = self.row.saturating_sub(first_param(params, 1) as usize),
            'B' => {
                let n = first_param(params, 1) as usize;
                self.row = (self.row + n).min(self.rows.len().saturating_sub(1));
            }
            'C' => self.col = (self.col + first_param(params, 1) as usize).min(MAX_COLS),
            'D' => self.col = self.col.saturating_sub(first_param(params, 1) as usize),
            'G' => {
                self.col = (first_param(params, 1) as usize)
                    .saturating_sub(1)
                    .min(MAX_COLS)
            }
            _ => {}
        }
    }
}

/// First parameter value, substituting `default` for a missing or `0` value (CSI cursor ops treat `0` as `1`; erase ops pass `0` as the default).
fn first_param(params: &Params, default: u16) -> u16 {
    match params.iter().next().and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

/// Map a 0-7 ANSI color index to a named ratatui color.
fn ansi16(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

/// Map a 0-7 bright ANSI color index to a named ratatui color.
fn ansi16_bright(n: u16) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Resolve an extended color (`38`/`48`) in either `;` (advancing `i` over the consumed groups) or `:` subparameter form.
/// Returns an un-quantized color.
fn ext_color(groups: &[&[u16]], i: &mut usize) -> Option<Color> {
    let g = groups.get(*i).copied()?;
    if g.len() >= 2 {
        let rest = g.get(1..)?;
        return parse_ext(rest);
    }
    match groups.get(*i + 1).and_then(|p| p.first().copied())? {
        5 => {
            let idx = groups.get(*i + 2).and_then(|p| p.first().copied())?;
            *i += 2;
            Some(Color::Indexed(idx as u8))
        }
        2 => {
            let r = groups.get(*i + 2).and_then(|p| p.first().copied())?;
            let g = groups.get(*i + 3).and_then(|p| p.first().copied())?;
            let b = groups.get(*i + 4).and_then(|p| p.first().copied())?;
            *i += 4;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

/// Parse the subparameter form of an extended color, e.g. `[5, n]` (256) or `[2, r, g, b]` (with an optional leading colorspace id).
/// Returns an un-quantized color.
fn parse_ext(sub: &[u16]) -> Option<Color> {
    match sub.first().copied()? {
        5 => sub.get(1).map(|n| Color::Indexed(*n as u8)),
        2 => {
            let vals = sub.get(1..)?;
            let (r, g, b) = match vals {
                [r, g, b] => (*r, *g, *b),
                [.., r, g, b] => (*r, *g, *b),
                _ => return None,
            };
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

fn row_to_line(cells: Vec<Cell>, base: Style) -> RenderedLine {
    let mut end = cells.len();
    while end > 0 {
        let Some(cell) = end.checked_sub(1).and_then(|i| cells.get(i)) else {
            break;
        };
        if cell.ch == ' ' && cell.style == base {
            end -= 1;
        } else {
            break;
        }
    }
    let Some(cells) = cells.get(..end) else {
        return RenderedLine {
            line: Line::default(),
            plain: String::new(),
        };
    };
    if cells.is_empty() {
        return RenderedLine {
            line: Line::default(),
            plain: String::new(),
        };
    }
    let plain: String = cells.iter().map(|c| c.ch).collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let Some(first) = cells.first() else {
        return RenderedLine {
            line: Line::default(),
            plain,
        };
    };
    let mut style = first.style;
    for c in cells {
        if c.style != style {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
            style = c.style;
        }
        buf.push(c.ch);
    }
    spans.push(Span::styled(buf, style));
    RenderedLine {
        line: Line::from(spans),
        plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nth<T>(xs: &[T], i: usize) -> &T {
        let Some(x) = xs.get(i) else {
            panic!("expected item {i}, got {} items", xs.len());
        };
        x
    }

    fn plain(raw: &str) -> String {
        render_terminal_plain(raw)
    }

    fn lines(raw: &str) -> Vec<String> {
        render_terminal_lines(raw, Style::default())
            .into_iter()
            .map(|rl| rl.plain)
            .collect()
    }

    #[test]
    fn strips_sgr_to_plain_text() {
        assert_eq!(plain("\x1b[1m\x1b[36mbazel\x1b[0m"), "bazel");
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        assert_eq!(plain("aaaa\rbb"), "bbaa");
    }

    #[test]
    fn progress_bar_collapses_to_final_state() {
        assert_eq!(plain("10%\r50%\r100%\n"), "100%");
    }

    #[test]
    fn newline_splits_lines() {
        assert_eq!(lines("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn trailing_newline_adds_no_blank_line() {
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn cursor_up_then_carriage_return_and_erase() {
        // Write two lines, move up, overwrite the start, erase to end of line.
        assert_eq!(lines("line1\nline2\x1b[A\rXX\x1b[K"), vec!["XX", "line2"]);
    }

    #[test]
    fn tab_advances_to_next_stop() {
        assert_eq!(plain("a\tb"), "a       b");
    }

    #[test]
    fn malformed_escape_does_not_panic() {
        assert_eq!(plain("\x1b[38;5mhi"), "hi");
        assert!(plain("\x1b[99999999999m\x1b[mok").contains("ok"));
    }

    #[test]
    fn sgr_splits_into_styled_spans() {
        let rendered = render_terminal_lines("plain \x1b[31mred\x1b[0m", Style::default());
        assert_eq!(rendered.len(), 1);
        let spans = &nth(&rendered, 0).line.spans;
        assert_eq!(spans.len(), 2);
        let [a, b] = spans.as_slice() else {
            panic!("expected two spans: {spans:?}");
        };
        assert_eq!(a.content.as_ref(), "plain ");
        assert_eq!(b.content.as_ref(), "red");
        assert!(b.style.fg.is_some());
        assert_eq!(nth(&rendered, 0).plain, "plain red");
    }

    #[test]
    fn idempotent_line_count() {
        let raw = "a\nb\x1b[32mc\x1b[0m\rd\ne";
        let first = render_terminal_lines(raw, Style::default()).len();
        let second = render_terminal_lines(raw, Style::default()).len();
        assert_eq!(first, second);
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(render_terminal_lines("", Style::default()).is_empty());
    }

    #[test]
    fn ansi16_mapping() {
        assert_eq!(ansi16(1), Color::Red);
        assert_eq!(ansi16(6), Color::Cyan);
        assert_eq!(ansi16_bright(2), Color::LightGreen);
        assert_eq!(ansi16_bright(7), Color::White);
    }

    #[test]
    fn ext_color_subparam_forms() {
        assert_eq!(parse_ext(&[5, 42]), Some(Color::Indexed(42)));
        assert_eq!(parse_ext(&[2, 10, 20, 30]), Some(Color::Rgb(10, 20, 30)));
        assert_eq!(parse_ext(&[2, 0, 10, 20, 30]), Some(Color::Rgb(10, 20, 30)));
        assert_eq!(parse_ext(&[2, 1]), None);
    }

    // Pipe capture is plain text plus line endings, never a ConPTY screen stream.
    // CRLF and unsupported controls (DEC modes, OSC, cursor save) must be ignored without corrupting text.

    #[test]
    fn windows_crlf_line_endings() {
        assert_eq!(lines("a\r\nb\r\nc\r\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn ignores_dec_private_modes_and_osc() {
        let raw = "\x1b[?25l\x1b]0;window title\x07hello\x1b[?1049h world\x1b[?25h";
        assert_eq!(plain(raw), "hello world");
    }

    #[test]
    fn ignores_cursor_save_restore_and_absolute_positioning() {
        assert_eq!(plain("\x1b7\x1b[10;5Hkept\x1b8"), "kept");
    }

    #[test]
    fn forced_sgr_over_crlf_renders_styled() {
        let rendered =
            render_terminal_lines("\x1b[01;31mmatch\x1b[0m\r\nplain\r\n", Style::default());
        assert_eq!(rendered.len(), 2);
        assert_eq!(nth(&rendered, 0).plain, "match");
        assert_eq!(nth(&rendered, 1).plain, "plain");
        assert!(
            nth(&rendered, 0)
                .line
                .spans
                .iter()
                .any(|s| s.style.fg.is_some())
        );
    }

    // Real Windows shell output samples
    // Each pins a distinct parser behavior exercised by a sequence these shells actually emit on the wire

    // Git Bash / GNU `grep --color=always` wraps the match in a bold-red SGR with an interleaved EL (`\x1b[K`)
    // An empty-param reset (`\x1b[m`) closes it
    // The EL must not truncate already-printed text, and `\x1b[m` must restore the base style for the trailing run
    #[test]
    fn git_bash_gnu_grep_color() {
        let rendered =
            render_terminal_lines("\x1b[01;31m\x1b[Kfoo\x1b[m\x1b[Kbar\n", Style::default());
        assert_eq!(rendered.len(), 1);
        assert_eq!(nth(&rendered, 0).plain, "foobar");
        let spans = &nth(&rendered, 0).line.spans;
        assert_eq!(spans.len(), 2);
        let [a, b] = spans.as_slice() else {
            panic!("expected two spans: {spans:?}");
        };
        assert_eq!(a.content.as_ref(), "foo");
        assert!(a.style.fg.is_some());
        assert!(a.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(b.content.as_ref(), "bar");
        assert_eq!(b.style, Style::default());
    }

    // PowerShell 7 (`$PSStyle`) emits 24-bit color in the semicolon form `\x1b[38;2;R;G;Bm`
    // That drives the `ext_color` branch that consumes the following groups and advances `i`
    // If that advance were wrong the trailing `0` param would reset and drop the color
    #[test]
    fn powershell_truecolor_psstyle() {
        let rendered = render_terminal_lines(
            "\x1b[38;2;255;128;0mWARNING\x1b[0m: low disk\n",
            Style::default(),
        );
        assert_eq!(rendered.len(), 1);
        assert_eq!(nth(&rendered, 0).plain, "WARNING: low disk");
        let spans = &nth(&rendered, 0).line.spans;
        let [a, b, ..] = spans.as_slice() else {
            panic!("expected two spans: {spans:?}");
        };
        assert_eq!(a.content.as_ref(), "WARNING");
        assert!(a.style.fg.is_some());
        assert_eq!(b.style, Style::default());
    }

    // Progress output (cargo/npm/pip style under cmd/PowerShell) wipes the status line with EL mode 2 (`\x1b[2K`) regardless of cursor column
    // The line is then rewritten, so the transcript collapses to the final line
    #[test]
    fn progress_erase_entire_line_collapses() {
        assert_eq!(lines("loading 99%\x1b[2K\rdone\n"), vec!["done"]);
    }
}
