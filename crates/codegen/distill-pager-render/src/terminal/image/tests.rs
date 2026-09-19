use super::*;

#[test]
fn protocol_matrix_matches_supported_terminals() {
    for (brand, expected) in [
        (TerminalName::Kitty, GraphicsProtocol::Kitty),
        (TerminalName::Ghostty, GraphicsProtocol::Kitty),
        (TerminalName::WezTerm, GraphicsProtocol::Kitty),
        (TerminalName::WarpTerminal, GraphicsProtocol::Kitty),
        (TerminalName::Iterm2, GraphicsProtocol::None),
        (TerminalName::Unknown, GraphicsProtocol::None),
    ] {
        assert_eq!(protocol_for_brand(brand, false), expected);
        assert_eq!(protocol_for_brand(brand, true), GraphicsProtocol::None);
    }
}

#[test]
fn prompt_preview_protocol_gates_iterm2_on_file_capability() {
    for (features, is_ssh, expected) in [
        (Some("TF"), false, GraphicsProtocol::ITerm2),
        (Some("T"), false, GraphicsProtocol::None),
        (None, false, GraphicsProtocol::None),
        (None, true, GraphicsProtocol::ITerm2),
        // A forwarded/inherited TERM_FEATURES without `F` still denies on SSH.
        (Some("T"), true, GraphicsProtocol::None),
    ] {
        assert_eq!(
            prompt_preview_protocol_for_brand(TerminalName::Iterm2, false, features, is_ssh),
            expected,
            "features={features:?} is_ssh={is_ssh}",
        );
    }
    // Windows (ConPTY strips the escapes) stays off even with the capability.
    assert_eq!(
        prompt_preview_protocol_for_brand(TerminalName::Iterm2, true, Some("F"), false),
        GraphicsProtocol::None,
    );
    // A stray `F` on a non-iTerm2 brand grants nothing.
    assert_eq!(
        prompt_preview_protocol_for_brand(TerminalName::Kitty, false, None, false),
        GraphicsProtocol::Kitty,
    );
    assert_eq!(
        prompt_preview_protocol_for_brand(TerminalName::Unknown, false, Some("F"), false),
        GraphicsProtocol::None,
    );
}

#[test]
fn cell_aspect_from_measures_and_rejects_bogus_reports() {
    use crossterm::terminal::WindowSize;
    let ws = |columns, rows, width, height| WindowSize {
        columns,
        rows,
        width,
        height,
    };
    // 10x20 px cells give a 0.5 ratio; 9x20 give 0.45
    assert_eq!(cell_aspect_from(&ws(100, 50, 1000, 1000)), 0.5);
    assert_eq!(cell_aspect_from(&ws(100, 50, 900, 1000)), 0.45);
    // Unreported pixels (zeroes) and out-of-band ratios fall back.
    assert_eq!(cell_aspect_from(&ws(100, 50, 0, 0)), DEFAULT_CELL_ASPECT);
    assert_eq!(cell_aspect_from(&ws(0, 0, 1000, 1000)), DEFAULT_CELL_ASPECT);
    assert_eq!(
        cell_aspect_from(&ws(100, 50, 5000, 1000)),
        DEFAULT_CELL_ASPECT,
        "square-or-wider cells are a bogus report"
    );
}

#[test]
fn iterm_overlay_escape_positions_then_restores_cursor() {
    let escape = build_overlay_image_escapes_for_protocol(
        GraphicsProtocol::ITerm2,
        &[0u8; 10],
        20,
        10,
        2,
        3,
    )
    .unwrap();
    assert!(escape.starts_with("\x1b7"));
    assert!(escape.ends_with("\x1b8"));
    assert!(escape.contains("\x1b[4;3H"));
    assert!(escape.contains("\x1b]1337;File="));
}

#[test]
fn scrollback_overlay_excludes_warp() {
    assert!(scrollback_inline_overlay_active_for_brand(
        GraphicsProtocol::Kitty,
        TerminalName::Kitty,
    ));
    assert!(!scrollback_inline_overlay_active_for_brand(
        GraphicsProtocol::Kitty,
        TerminalName::WarpTerminal,
    ));
}

#[test]
#[serial_test::serial]
fn force_off_overrides_capability() {
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    set_inline_overlay_force_off(false);
    assert!(scrollback_inline_overlay_active());
    set_inline_overlay_force_off(true);
    assert!(!scrollback_inline_overlay_active());
    set_inline_overlay_force_off(false);
}

#[test]
fn kitty_escape_chunks_and_preserves_cursor() {
    let small = render_kitty_image(&[0u8; 10], KittyImageFormat::Png, 40, 20);
    assert!(small.contains("a=T"));
    assert!(small.contains("f=100"));
    assert!(small.contains("q=2"));
    assert!(small.contains("C=1"));
    assert!(small.contains("c=40"));
    assert!(small.contains("r=20"));
    assert!(small.contains("m=0"));
    let large = render_kitty_image(&vec![0u8; 5000], KittyImageFormat::Png, 40, 20);
    assert!(large.matches("\x1b_G").count() > 1);
}

#[test]
fn kitty_format_and_conversion_produce_png() {
    use image::{ImageBuffer, Rgb};

    let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    assert_eq!(kitty_format_from_bytes(&png), Some(KittyImageFormat::Png));
    let buffer: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(4, 3, Rgb([128, 64, 32]));
    let mut jpeg = Vec::new();
    buffer
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    assert_eq!(kitty_format_from_bytes(&jpeg), None);
    let converted = prepare_kitty_overlay_image_bytes(&jpeg).unwrap();
    assert_eq!(
        kitty_format_from_bytes(&converted),
        Some(KittyImageFormat::Png)
    );
}

#[test]
fn iterm_escape_fills_requested_geometry_exactly() {
    let escape = render_iterm2_image(&[0u8; 10], 30, 15);
    assert!(escape.starts_with("\x1b]1337;File="));
    assert!(escape.contains("width=30cells"));
    assert!(escape.contains("height=15cells"));
    assert!(escape.contains("preserveAspectRatio=0"));
}

#[test]
fn low_level_overlay_deletes_then_transmits() {
    let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let escape =
        build_overlay_image_escapes_for_protocol(GraphicsProtocol::Kitty, &png, 20, 10, 0, 0)
            .unwrap();
    assert!(escape.contains("a=d,d=i"));
    assert!(escape.contains("a=T"));
    assert!(!escape.contains("a=t,"));
    assert!(!escape.contains("a=p"));
}

#[test]
fn iterm_place_can_skip_inline_data() {
    let _guard = set_protocol_for_test(GraphicsProtocol::ITerm2);
    let area = ratatui::layout::Rect::new(0, 0, 40, 20);
    let escape = place_inline_image(&[0u8; 10], 100, 50, area, 20, 0, 2, false).unwrap();
    assert!(escape.starts_with("\x1b["));
    assert!(!escape.contains("1337"));
}

#[test]
fn unchanged_static_overlay_skips_payload() {
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    crate::terminal::overlay::reset_owner();
    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    png.extend(std::iter::repeat_n(0u8, 200_000));
    let first = crate::terminal::overlay::static_image(&png, 40, 20, 0, 0, 9).unwrap();
    assert!(first.as_str().len() > 200_000);
    let _ = first.commit();
    let subsequent = crate::terminal::overlay::static_image(&png, 40, 20, 0, 0, 9).unwrap();
    assert!(subsequent.as_str().is_empty());
}
