//! X10 mouse reassembly filter for the input event channel.
//! Sibling of [`super::csi_filter`], which reassembles SGR-format text fragments.
//! See [`X10ReassemblyFilter`].

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent};

use super::event_loop::TimedInputEvent;

/// These coordinates are fixed by the encoding and never reflect the true position.
/// Terminal bounds therefore cannot distinguish a mangled report from a genuine event on a large terminal.
/// The pair decodes from contiguous bytes in a single terminal read, so the completion must arrive within [`MAX_COMPLETION_GAP`] of the candidate.
pub(super) struct X10ReassemblyFilter {
    /// Candidate mangled report held awaiting its displaced row byte.
    held: Option<HeldReport>,
}

/// A magic-shape mouse event held as a mangled-report candidate; typed so reconstruction cannot see a non-mouse event.
struct HeldReport {
    mouse: MouseEvent,
    arrived_at: Instant,
}

/// Maximum arrival gap between the mangled mouse event and its displaced row byte.
/// The pair is parsed from adjacent bytes of one read, so real pairs arrive effectively simultaneously.
/// Human typing after a coincidentally magic-shaped genuine event is orders of magnitude slower.
const MAX_COMPLETION_GAP: Duration = Duration::from_millis(50);

impl X10ReassemblyFilter {
    pub(super) fn new() -> Self {
        Self { held: None }
    }

    /// Process a batch of events, reassembling mangled X10 reports.
    /// A held candidate persists across calls so a pair the reader thread split across two batches is still reconstructed.
    pub(super) fn filter(&mut self, events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
        let mut result = Vec::with_capacity(events.len());
        let mut reassembled_count = 0usize;

        for ev in events {
            if let Some(held) = self.held.take() {
                let within_gap =
                    ev.arrived_at.saturating_duration_since(held.arrived_at) <= MAX_COMPLETION_GAP;
                match displaced_row_byte(&ev.event) {
                    Some(row_byte) if within_gap => {
                        result.push(reconstruct(&held, row_byte));
                        reassembled_count += 1;
                        continue;
                    }
                    _ => result.push(TimedInputEvent {
                        event: Event::Mouse(held.mouse),
                        arrived_at: held.arrived_at,
                    }),
                }
            }

            match ev.event {
                Event::Mouse(m) if is_mangled_shape(&m) => {
                    self.held = Some(HeldReport {
                        mouse: m,
                        arrived_at: ev.arrived_at,
                    });
                }
                _ => result.push(ev),
            }
        }

        if reassembled_count > 0 {
            tracing::debug!(reassembled_count, "reassembled mangled X10 mouse reports");
        }

        result
    }
}

/// Whether this mouse event matches the mis-parse shape.
/// The column is the UTF-8 lead byte (`0xC2`/`0xC3` minus 33) and the row is a continuation byte (`0x80..=0xBF` minus 33).
/// The shape is independent of the true position.
fn is_mangled_shape(m: &MouseEvent) -> bool {
    matches!(m.column, 161 | 162) && (95..=158).contains(&m.row)
}

/// The displaced row byte, if this event is one: a bare printable/Latin-1 `Char` press, or `Backspace` for row byte `0x7F`.
/// Crossterm reports uppercase coordinate bytes with SHIFT.
/// Coordinate bytes are always >= 33 (1-based coordinate + 32).
fn displaced_row_byte(event: &Event) -> Option<u16> {
    let Event::Key(ke) = event else {
        return None;
    };
    if ke.kind != KeyEventKind::Press
        || !(ke.modifiers == KeyModifiers::NONE || ke.modifiers == KeyModifiers::SHIFT)
    {
        return None;
    }
    match ke.code {
        KeyCode::Char(c) if (0x21..=0xFF).contains(&(c as u32)) => Some(c as u16),
        KeyCode::Backspace if ke.modifiers == KeyModifiers::NONE => Some(0x7F),
        _ => None,
    }
}

/// Invert the mis-parse: recombine the UTF-8 pair crossterm split across the held event's coordinates into the true column.
/// Decode the displaced byte as the true row (both 0-based, mirroring crossterm's `-33`).
fn reconstruct(held: &HeldReport, row_byte: u16) -> TimedInputEvent {
    let lead = held.mouse.column + 33;
    let continuation = held.mouse.row + 33;
    let column_byte = ((lead & 0x03) << 6) | (continuation & 0x3F);
    TimedInputEvent {
        event: Event::Mouse(MouseEvent {
            kind: held.mouse.kind,
            column: column_byte - 33,
            row: row_byte - 33,
            modifiers: held.mouse.modifiers,
        }),
        arrived_at: held.arrived_at,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use crossterm::event::{KeyEvent, KeyEventState, MouseButton, MouseEventKind};

    use super::*;

    fn nth<T>(xs: &[T], i: usize) -> &T {
        let Some(x) = xs.get(i) else {
            panic!("expected index {i}, len {}", xs.len());
        };
        x
    }

    fn test_instant() -> Instant {
        static NOW: OnceLock<Instant> = OnceLock::new();
        *NOW.get_or_init(Instant::now)
    }

    fn timed(event: Event) -> TimedInputEvent {
        TimedInputEvent {
            event,
            arrived_at: test_instant(),
        }
    }

    fn press_mods(code: KeyCode, modifiers: KeyModifiers) -> TimedInputEvent {
        timed(Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> TimedInputEvent {
        timed(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }))
    }

    /// The mis-parse of a motion report at column 100 (byte 0x84, expanded to C2 84): column = 0xC2 - 33 = 161, row = 0x84 - 33 = 99.
    fn mangled_c2_col100() -> TimedInputEvent {
        mouse(MouseEventKind::Moved, 161, 99)
    }

    #[test]
    fn reconstructs_c2_report_in_one_batch() {
        // Displaced row byte 'P' (0x50): true row 0x50 - 33 = 47.
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            nth(&out, 0).event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 99,
                row: 47,
                modifiers: KeyModifiers::empty(),
            })
        );
    }

    #[test]
    fn reconstructs_c3_report() {
        // 0xC3 lead (column byte 0xC0..=0xFF): mis-parse column 162
        // With continuation 0x84 (row 99), the true column byte is (0x03 << 6) | 0x04 = 0xC4, so column 163
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 162, 99),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(nth(&out, 0).event, Event::Mouse(m) if m.column == 163 && m.row == 47));
    }

    #[test]
    fn reconstructs_non_moved_kind() {
        // Left-button drag (CB byte 0x40): the kind survives the mis-parse and must survive reconstruction
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Drag(MouseButton::Left), 161, 99),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            nth(&out, 0).event,
            Event::Mouse(m) if m.kind == MouseEventKind::Drag(MouseButton::Left)
                && m.column == 99
                && m.row == 47
        ));
    }

    #[test]
    fn reconstructs_shape_that_is_in_bounds_on_large_terminals() {
        // On a 180x120 terminal the C3 mis-parse (162, 115) is inside the terminal's bounds
        // The shape is decisive regardless: reassembly must not be skipped
        // Regression test: the earlier bounds-check design let exactly this leak through
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 162, 115),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(nth(&out, 0).event, Event::Mouse(m) if m.column == 179 && m.row == 47));
    }

    #[test]
    fn reconstructs_report_split_across_batches() {
        let mut f = X10ReassemblyFilter::new();
        assert!(f.filter(vec![mangled_c2_col100()]).is_empty());
        let out = f.filter(vec![press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT)]);
        assert_eq!(out.len(), 1);
        assert!(matches!(nth(&out, 0).event, Event::Mouse(m) if m.column == 99 && m.row == 47));
    }

    #[test]
    fn reconstructs_latin1_row_byte() {
        // Both coordinates expanded: the displaced row byte itself arrives as a Latin-1 char (0xA0 gives row 127)
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char('\u{A0}'), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(nth(&out, 0).event, Event::Mouse(m) if m.column == 99 && m.row == 127));
    }

    #[test]
    fn reconstructs_backspace_row_byte() {
        // Row byte 0x7F parses as Backspace: true row 127 - 33 = 94.
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Backspace, KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(nth(&out, 0).event, Event::Mouse(m) if m.column == 99 && m.row == 94));
    }

    #[test]
    fn stale_completion_is_not_consumed() {
        // A keystroke arriving long after the candidate is real typing, not the displaced row byte: both events must survive
        let mut f = X10ReassemblyFilter::new();
        assert!(f.filter(vec![mangled_c2_col100()]).is_empty());
        let late_key = TimedInputEvent {
            arrived_at: test_instant() + Duration::from_millis(200),
            ..press_mods(KeyCode::Char('q'), KeyModifiers::NONE)
        };
        let out = f.filter(vec![late_key]);
        assert_eq!(out.len(), 2);
        assert_eq!(nth(&out, 0).event, mangled_c2_col100().event);
        assert!(matches!(
            nth(&out, 1).event,
            Event::Key(k) if k.code == KeyCode::Char('q')
        ));
    }

    #[test]
    fn candidate_followed_by_other_event_is_released() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            timed(Event::FocusGained),
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE),
        ]);
        // Candidate released unchanged; the focus event and the (now unrelated) keystroke pass through
        assert_eq!(out.len(), 3);
        assert_eq!(nth(&out, 0).event, mangled_c2_col100().event);
        assert_eq!(nth(&out, 1).event, Event::FocusGained);
        assert_eq!(
            nth(&out, 2).event,
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE).event
        );
    }

    #[test]
    fn coordinates_outside_the_magic_shape_are_untouched() {
        // Column not a UTF-8 lead, or row outside the continuation range: never held, following keystroke types normally
        for candidate in [
            mouse(MouseEventKind::Moved, 160, 99),
            mouse(MouseEventKind::Moved, 163, 99),
            mouse(MouseEventKind::Moved, 161, 94),
            mouse(MouseEventKind::Moved, 161, 159),
        ] {
            let out = X10ReassemblyFilter::new().filter(vec![
                candidate,
                press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
            ]);
            assert_eq!(out.len(), 2);
        }
    }

    #[test]
    fn plain_typing_is_untouched() {
        let out = X10ReassemblyFilter::new().filter(vec![
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ordinary_mouse_events_are_untouched() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 80, 20),
            mouse(MouseEventKind::ScrollUp, 119, 49),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn non_coordinate_key_does_not_complete() {
        // Byte range check: control-range chars (< 0x21) can never be a coordinate byte, so the candidate is released
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char(' '), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 2);
    }
}
