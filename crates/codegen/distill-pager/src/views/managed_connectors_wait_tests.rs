use super::*;
use pretty_assertions::assert_eq;

const TEAM: &str = "7c1164fe-9c32-465c-ba46-a08f28bf7679";

fn painted_state() -> ManagedConnectorsWaitState {
    ManagedConnectorsWaitState {
        copy_rect: Some(Rect::new(10, 5, 14, 1)),
        url_rects: vec![Rect::new(2, 7, 30, 1), Rect::new(2, 8, 12, 1)],
        ..ManagedConnectorsWaitState::new(Some(TEAM))
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn no_copy(_: &str) -> ClipboardDelivery {
    panic!("clipboard must not be touched")
}

#[test]
fn new_state_fixes_the_url_for_the_wait() {
    assert_eq!(
        &*ManagedConnectorsWaitState::new(Some(TEAM)).url,
        managed_connectors_url(Some(TEAM))
    );
    assert_eq!(
        &*ManagedConnectorsWaitState::new(None).url,
        managed_connectors_url(None)
    );
}

#[test]
fn key_r_refreshes_esc_dismisses_ctrl_o_reopens_everything_else_is_swallowed() {
    let wait = ManagedConnectorsWaitState::new(None);
    assert_eq!(
        wait.handle_key(&key(KeyCode::Char('r'))),
        ManagedConnectorsWaitOutcome::Refresh
    );
    assert_eq!(
        wait.handle_key(&key(KeyCode::Char('R'))),
        ManagedConnectorsWaitOutcome::Refresh
    );
    assert_eq!(
        wait.handle_key(&key(KeyCode::Esc)),
        ManagedConnectorsWaitOutcome::Dismiss
    );
    assert_eq!(
        wait.handle_key(&KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
        ManagedConnectorsWaitOutcome::OpenConnectors
    );
    for code in [
        KeyCode::Char('o'),
        KeyCode::Char(' '),
        KeyCode::Char('x'),
        KeyCode::Char('a'),
        KeyCode::Char('i'),
        KeyCode::Enter,
        KeyCode::Down,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Left,
        KeyCode::Right,
    ] {
        assert_eq!(
            wait.handle_key(&key(code)),
            ManagedConnectorsWaitOutcome::Ignored,
            "{code:?} must not reach the list under the overlay"
        );
    }
}

#[test]
fn short_overlay_drops_spacers_before_the_copy_button_and_url() {
    let theme = Theme::current();
    let mut wait = ManagedConnectorsWaitState::new(Some(TEAM));
    let url_rows = word_wrap(&wait.url, 60).len();
    assert_eq!(url_rows, 2, "team URL wraps to two rows at this width");

    // Three text/button rows plus two URL rows fit only once both spacers are gone.
    let area = Rect::new(0, 0, 60, 5);
    let mut buf = Buffer::empty(area);
    render_managed_connectors_wait(&mut buf, area, &mut wait, &theme);
    assert!(
        wait.copy_rect.is_some(),
        "copy button must survive a short overlay"
    );
    assert_eq!(wait.url_rects.len(), 2, "both URL rows must be painted");
    let painted = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(
        painted.iter().all(|row| !row.is_empty()),
        "no blank spacer row may remain while content is cut: {painted:?}"
    );

    // With room to spare the spacers stay: seven lines centered in twelve rows start at row 2,
    // with the two spacers at rows 3 and 5.
    let mut wait = ManagedConnectorsWaitState::new(Some(TEAM));
    let tall = Rect::new(0, 0, 60, 12);
    let mut buf = Buffer::empty(tall);
    render_managed_connectors_wait(&mut buf, tall, &mut wait, &theme);
    let painted_rows: Vec<u16> = (0..tall.height)
        .filter(|&y| {
            (0..tall.width).any(|x| {
                buf.cell((x, y))
                    .is_some_and(|c| !c.symbol().trim().is_empty())
            })
        })
        .collect();
    assert_eq!(painted_rows, [2, 4, 6, 7, 8]);
    assert_eq!(wait.copy_rect.map(|r| r.y), Some(6));
    assert_eq!(
        wait.url_rects.iter().map(|r| r.y).collect::<Vec<_>>(),
        [7, 8]
    );
}

#[test]
fn hover_flips_copy_hovered_only_on_change() {
    let mut wait = painted_state();
    let onto = mouse(MouseEventKind::Moved, 12, 5);
    assert_eq!(
        wait.handle_mouse(&onto, no_copy),
        ManagedConnectorsWaitOutcome::Changed
    );
    assert!(wait.copy_hovered);
    assert_eq!(
        wait.handle_mouse(&onto, no_copy),
        ManagedConnectorsWaitOutcome::Ignored,
        "no redraw when hover state is unchanged"
    );
    let away = mouse(MouseEventKind::Moved, 40, 20);
    assert_eq!(
        wait.handle_mouse(&away, no_copy),
        ManagedConnectorsWaitOutcome::Changed
    );
    assert!(!wait.copy_hovered);
}

#[test]
fn copy_click_copies_the_wait_url_and_records_delivery() {
    let mut wait = painted_state();
    let click = mouse(MouseEventKind::Down(MouseButton::Left), 20, 5);
    let mut seen = None;
    let outcome = wait.handle_mouse(&click, |text| {
        seen = Some(text.to_owned());
        ClipboardDelivery::Confirmed
    });
    assert_eq!(outcome, ManagedConnectorsWaitOutcome::Changed);
    assert_eq!(
        seen.as_deref(),
        Some(managed_connectors_url(Some(TEAM)).as_str())
    );
    assert!(wait.url_copied);

    let outcome = wait.handle_mouse(&click, |_| ClipboardDelivery::Failed);
    assert_eq!(outcome, ManagedConnectorsWaitOutcome::Changed);
    assert!(!wait.url_copied, "a failed copy must not show [copied]");
}

#[test]
fn url_click_reopens_connectors_on_any_wrapped_row() {
    let mut wait = painted_state();
    for (col, row) in [(2, 7), (31, 7), (13, 8)] {
        let click = mouse(MouseEventKind::Down(MouseButton::Left), col, row);
        assert_eq!(
            wait.handle_mouse(&click, no_copy),
            ManagedConnectorsWaitOutcome::OpenConnectors,
            "({col},{row}) is inside a URL rect"
        );
    }
    // One past the end of the second row: outside every rect.
    let miss = mouse(MouseEventKind::Down(MouseButton::Left), 14, 8);
    assert_eq!(
        wait.handle_mouse(&miss, no_copy),
        ManagedConnectorsWaitOutcome::Changed
    );
}

#[test]
fn clicks_outside_targets_are_absorbed_not_ignored() {
    let mut wait = painted_state();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Middle),
    ] {
        assert_eq!(
            wait.handle_mouse(&mouse(kind, 50, 15), no_copy),
            ManagedConnectorsWaitOutcome::Changed,
            "{kind:?} must be absorbed so the row underneath cannot fire"
        );
    }
    for kind in [
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
    ] {
        assert_eq!(
            wait.handle_mouse(&mouse(kind, 50, 15), no_copy),
            ManagedConnectorsWaitOutcome::Ignored
        );
    }
}
