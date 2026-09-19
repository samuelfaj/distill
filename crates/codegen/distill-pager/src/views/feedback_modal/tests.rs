// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::input::line_editor::LineEditor;
use crate::prompt_images::PastedImage;
use crate::theme::Theme;
use distill_ratatui_textarea::ElementId;

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn open_with_text(text: &str) -> FeedbackModalState {
    FeedbackModalState::new(OpenFeedbackModal {
        text: Some(text.to_string()),
        ..Default::default()
    })
}

fn test_image(display_number: usize) -> PastedImage {
    PastedImage {
        element_id: ElementId::from_raw(0),
        display_number,
        mime_type: "image/png".to_string(),
        dimensions: Some((16, 16)),
        byte_len: 4,
        encoded_bytes: Some(vec![1, 2, 3, 4].into()),
        source_path: None,
        staged_temp_path: None,
        session_image_path: None,
        preview: crate::prompt_images::PromptImagePreview::default(),
    }
}

fn draft(id: &str, details: &str, taxonomy: FeedbackTaxonomy) -> FeedbackDraft {
    FeedbackDraft {
        id: id.to_owned().into(),
        title: details.lines().next().unwrap_or("Draft").to_owned(),
        details: details.to_owned(),
        area: None,
        r#type: Some(taxonomy.r#type.unwrap_or(FeedbackType::Bug)),
        task_category: taxonomy.task_category,
        failure_mode: taxonomy.failure_mode,
        created_at: 1,
        revision: 1,
    }
}

fn buffer_text(buf: &Buffer) -> String {
    (buf.area.top()..buf.area.bottom())
        .map(|y| {
            (buf.area.left()..buf.area.right())
                .filter_map(|x| buf.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_of(rendered: &str, label: &str) -> usize {
    rendered
        .lines()
        .position(|line| line.contains(label))
        .unwrap_or_else(|| panic!("{label} must render on its own row"))
}

fn left_click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn render_uses_feedback_chrome_and_write_tab() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_text("rendered report");

    let frame = modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");

    let rendered = buffer_text(&buf);
    assert!(rendered.contains("Feedback"));
    assert!(rendered.contains("Write"));
    assert!(rendered.contains("Drafts"));
    assert!(rendered.contains("rendered report"));
    assert!(modal.window.popup_area.is_some());
    assert_eq!(modal.window.tab_count, 2);
    assert!(
        frame.cursor.is_some(),
        "Write composer must expose a terminal caret"
    );
}

#[test]
fn narrow_render_preserves_state() {
    let area = Rect::new(0, 0, 18, 5);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_text("keep me");

    modal.render(&mut buf, area, &Theme::current(), false);

    assert_eq!(modal.text(), "keep me");
    assert!(modal.window.popup_area.is_none());
}

#[test]
fn wrap_image_paste_targets_the_feedback_composer() {
    let mut agent = crate::app::agent_view::test_fixtures::make_agent();
    agent.feedback_modal = Some(open_with_text("report"));
    let image = crate::clipboard::ImageData {
        data: {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode("iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAADUlEQVR4nGP4z4AHAAk4AQB4xZKtAAAAAElFTkSuQmCC")
                .unwrap()
        },
        mime_type: "image/png".to_string(),
    };
    let encoded = crate::wrap_clipboard_image::encode_wrap_image_response(Some(&image));
    let paste = String::from_utf8(encoded)
        .unwrap()
        .strip_prefix("\u{1b}[200~")
        .and_then(|text| text.strip_suffix("\u{1b}[201~"))
        .unwrap()
        .to_string();

    let outcome = agent.handle_input(
        &crossterm::event::Event::Paste(paste),
        &crate::actions::ActionRegistry::defaults(),
    );

    assert!(matches!(
        outcome,
        crate::app::app_view::InputOutcome::Changed
    ));
    assert_eq!(agent.feedback_modal.as_ref().unwrap().image_count(), 1);
    assert!(agent.prompt.images.is_empty());
}

#[test]
fn at_sign_stays_plain_text_without_file_search() {
    let mut modal = open_with_text("");

    assert_eq!(
        modal.handle_key(&key(KeyCode::Char('@'), KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(modal.text(), "@");
    assert!(!modal.composer.file_search_visible());
    assert!(modal.composer.pending_viewer_request.is_none());
}

#[test]
fn enter_submits_nonblank_text_and_modified_enter_inserts_a_newline() {
    let enter = key(KeyCode::Enter, KeyModifiers::NONE);
    let mut modal = open_with_text("useful feedback");
    assert_eq!(modal.handle_key(&enter), FeedbackModalOutcome::Submit);

    let mut blank = open_with_text(" \n ");
    assert_eq!(blank.handle_key(&enter), FeedbackModalOutcome::Changed);
    assert!(!blank.is_sendable());

    for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
        let mut modal = open_with_text("first");
        assert_eq!(
            modal.handle_key(&key(KeyCode::Enter, modifiers)),
            FeedbackModalOutcome::Changed
        );
        assert_eq!(modal.text(), "first\n");
    }
}

#[test]
fn backslash_enter_continues_instead_of_submitting() {
    let mut modal = open_with_text("continue\\");

    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(modal.text(), "continue\n");
}

#[test]
fn apple_modifier_seam_inserts_newline() {
    let mut modal = open_with_text("first");
    let outcome = modal
        .composer_mut()
        .route_enter_with_newline_modifier(&key(KeyCode::Enter, KeyModifiers::NONE), true);

    assert_eq!(outcome, EnterOutcome::NewlineInserted);
    assert_eq!(modal.text(), "first\n");
}

#[test]
fn image_seed_appends_live_chips_after_prefill_text() {
    let modal = FeedbackModalState::new(OpenFeedbackModal {
        text: Some("restored draft".to_string()),
        images: vec![test_image(4), test_image(7)].into(),
        ..Default::default()
    });

    assert_eq!(modal.image_count(), 2);
    assert!(modal.text().starts_with("restored draft"));
    assert!(modal.text().contains("[Image #4]"));
    assert!(modal.text().contains("[Image #7]"));
    assert!(!modal.submitted_text().contains("[Image #"));
}

#[test]
fn image_seed_enforces_cap_and_image_only_can_submit() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal {
        images: (1..=PromptWidget::IMAGE_CAP + 2)
            .map(test_image)
            .collect::<Vec<_>>()
            .into(),
        ..Default::default()
    });

    assert_eq!(modal.image_count(), PromptWidget::IMAGE_CAP);
    assert!(modal.is_sendable());
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Submit
    );
}

fn open_with_all_metadata(text: &str) -> FeedbackModalState {
    FeedbackModalState::new(OpenFeedbackModal {
        text: Some(text.to_string()),
        r#type: Some(FeedbackType::Bug),
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode: Some(FeedbackFailureMode::SloppyCode),
        draft_id: Some("opaque-draft".to_string().into()),
        ..Default::default()
    })
}

/// One wide render so the label rows and composer geometry exist: keyboard entry to the rows
/// (Tab, Up-from-the-top) is gated on rows the last render actually drew.
fn render_wide(modal: &mut FeedbackModalState) -> FeedbackModalRender {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal")
}

#[test]
fn supplied_metadata_renders_labels_but_never_the_draft_id() {
    // 50 columns could not fit the three labels side by side; stacked rows each fit in full.
    let area = Rect::new(0, 0, 50, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("labeled report");

    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("clamped width should still render the modal");

    let rendered = buffer_text(&buf);
    let type_row = row_of(&rendered, "Type: Bug");
    let task_row = row_of(&rendered, "Task: Debug");
    let failure_row = row_of(&rendered, "Failure: Code quality");
    assert_eq!(task_row, type_row + 1, "one full-width row per enum");
    assert_eq!(failure_row, task_row + 1, "one full-width row per enum");
    assert!(rendered.contains("labeled report"));
    // The draft id is carried opaquely; rendering it would leak a behaviorless internal handle.
    assert!(!rendered.contains("opaque-draft"));
}

#[test]
fn absent_metadata_renders_a_neutral_write_step() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_text("plain report");

    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");

    let rendered = buffer_text(&buf);
    for label in ["Type:", "Task:", "Failure:"] {
        assert!(!rendered.contains(label), "{label} must stay absent");
    }
    assert!(rendered.contains("plain report"));
}

#[test]
fn tight_height_drops_the_label_rows_before_the_composer() {
    // v_margin(14) + borders(2) + tabs/divider(2) + footer(2) leaves a 2-row content area: below
    // the three label rows plus the composer's two-row reserve.
    let area = Rect::new(0, 0, 100, 22);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("tight");

    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("short-but-valid area should render the modal");

    let rendered = buffer_text(&buf);
    assert!(
        !rendered.contains("Type: Bug"),
        "labels yield to the composer when rows run out"
    );
    assert!(rendered.contains("tight"));
    assert_eq!(modal.text(), "tight");
}

/// The envelope shape and wire spellings are the crate's contract; this pins only what the modal
/// decides: `source` follows the draft origin and the supplied enums are forwarded.
#[test]
fn modal_metadata_forwards_source_and_taxonomy() {
    assert_eq!(
        open_with_all_metadata("enveloped").structured_feedback_metadata(),
        structured_feedback(
            FeedbackSource::Draft,
            FeedbackTaxonomy {
                r#type: Some(FeedbackType::Bug),
                task_category: Some(FeedbackTaskCategory::Debug),
                failure_mode: Some(FeedbackFailureMode::SloppyCode),
            },
        )
    );

    let partial = FeedbackModalState::new(OpenFeedbackModal {
        r#type: Some(FeedbackType::Idea),
        ..Default::default()
    });
    assert_eq!(
        partial.structured_feedback_metadata(),
        structured_feedback(
            FeedbackSource::Write,
            FeedbackTaxonomy {
                r#type: Some(FeedbackType::Idea),
                ..FeedbackTaxonomy::default()
            },
        )
    );

    // The opaque draft_id decides only `source`.
    assert_eq!(
        open_with_text("plain").structured_feedback_metadata(),
        structured_feedback(FeedbackSource::Write, FeedbackTaxonomy::default())
    );
    let draft_only = FeedbackModalState::new(OpenFeedbackModal {
        draft_id: Some("opaque".to_string().into()),
        ..Default::default()
    });
    assert_eq!(
        draft_only.structured_feedback_metadata(),
        structured_feedback(FeedbackSource::Draft, FeedbackTaxonomy::default())
    );
}

// -- Editable label rows: focus, navigation, and styling --

#[test]
fn tab_focuses_the_label_rows_and_up_down_move_between_them() {
    let mut modal = open_with_all_metadata("editable");
    render_wide(&mut modal);

    assert_eq!(
        modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(modal.metadata_focus, Some(MetadataField::Type));
    assert!(
        render_wide(&mut modal).cursor.is_none(),
        "the composer caret is the composer-focus signal; a focused row hides it"
    );
    // Down walks the stacked rows; Up walks back and stops at the top instead of wrapping.
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, Some(MetadataField::Task));
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, Some(MetadataField::Failure));
    modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, Some(MetadataField::Type));
    // Up/Down never edit a value: only Left/Right and the picker write.
    assert_eq!(modal.metadata().r#type, Some(FeedbackType::Bug));
    assert_eq!(
        modal.metadata().task_category,
        Some(FeedbackTaskCategory::Debug)
    );
    // Tab hands the keys back: typing edits the draft again.
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(modal.text(), "editable!");
}

#[test]
fn left_right_cycle_the_focused_rows_value_with_wrap() {
    let mut modal = open_with_all_metadata("cycle");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, Some(MetadataField::Type));

    // Right steps to the next variant in the field's fixed ALL order.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Right, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(modal.metadata().r#type, Some(FeedbackType::Idea));
    // The cycled value renders in place: only the Type row's text changed, the rows stay put.
    let rendered = render_to_text(&mut modal);
    let type_row = row_of(&rendered, "Type: Idea");
    assert_eq!(row_of(&rendered, "Task: Debug"), type_row + 1);
    assert_eq!(row_of(&rendered, "Failure: Code quality"), type_row + 2);
    // Left steps back to the previous variant.
    modal.handle_key(&key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(modal.metadata().r#type, Some(FeedbackType::Bug));
    // Left off the first variant wraps to the last...
    modal.handle_key(&key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(
        modal.metadata().r#type,
        Some(FeedbackType::MissingCapability)
    );
    // ...and Right off the last wraps back to the first.
    modal.handle_key(&key(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(modal.metadata().r#type, Some(FeedbackType::Bug));

    // Cycling edits the focused field only, never the row order or the focus.
    assert_eq!(
        modal.field_order,
        vec![
            MetadataField::Type,
            MetadataField::Task,
            MetadataField::Failure
        ]
    );
    assert_eq!(modal.metadata_focus, Some(MetadataField::Type));
    assert_eq!(
        modal.metadata().task_category,
        Some(FeedbackTaskCategory::Debug)
    );
    assert_eq!(
        modal.metadata().failure_mode,
        Some(FeedbackFailureMode::SloppyCode)
    );
}

/// The label text must share the composer draft's left edge; the focus marker sits in the
/// modal's padding gutter, so focusing never shoves the words sideways.
#[test]
fn label_text_left_aligns_with_the_composer_draft() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("aligned draft");
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");

    let col_of = |rendered: &str, needle: &str| {
        rendered
            .lines()
            .find_map(|line| {
                line.find(needle)
                    .and_then(|idx| line.get(..idx).map(|s| s.chars().count()))
            })
            .unwrap_or_else(|| panic!("{needle} must render"))
    };
    let rendered = buffer_text(&buf);
    let draft_col = col_of(&rendered, "aligned draft");
    assert_eq!(col_of(&rendered, "Type: Bug"), draft_col);
    assert_eq!(col_of(&rendered, "Task: Debug"), draft_col);
    assert_eq!(col_of(&rendered, "Failure: Code quality"), draft_col);

    // Focusing a row draws the marker left of the text; the words stay at the draft's column.
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    let mut buf = Buffer::empty(area);
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");
    let rendered = buffer_text(&buf);
    assert_eq!(col_of(&rendered, "Type: Bug"), draft_col);
    assert!(
        rendered
            .lines()
            .any(|line| line.contains('\u{276F}') && line.contains("Type: Bug")),
        "the focused row carries the gutter marker"
    );
}

#[test]
fn up_from_the_composer_top_focuses_the_bottom_label_row() {
    let mut modal = open_with_all_metadata("draft");
    render_wide(&mut modal);

    // An unwrapped single-line draft keeps the caret on the top visual row wherever it sits.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(
        modal.metadata_focus,
        Some(MetadataField::Failure),
        "the bottom row is the natural landing spot above the composer"
    );

    // Down off the bottom row returns to the composer: typing edits the draft.
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, None);
    modal.handle_key(&key(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(modal.text(), "draft!");
}

#[test]
fn up_below_the_top_visual_row_moves_the_caret_before_the_labels() {
    // The caret opens at the end of the draft, below the top visual row. Only the two-line case
    // has an exact hop count; the wrapped one depends on the composer width.
    for (label, text, expected_hops) in [
        ("two lines", "first\nsecond".to_owned(), 1..=1),
        ("wrapped line", "wrap ".repeat(80), 2..=31),
    ] {
        let mut modal = open_with_all_metadata(&text);
        render_wide(&mut modal);
        assert!(
            !modal.composer.caret_on_top_visual_row(),
            "{label}: the caret starts below the top visual row"
        );

        // Up moves the caret one visual row at a time; the labels never steal it mid-draft.
        let mut hops = 0;
        while !modal.composer.caret_on_top_visual_row() {
            assert_eq!(
                modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE)),
                FeedbackModalOutcome::Changed,
                "{label}"
            );
            assert_eq!(
                modal.metadata_focus, None,
                "{label}: Up below the top visual row must stay in the composer"
            );
            hops += 1;
            assert!(
                hops < 32,
                "{label}: the caret must reach the top visual row"
            );
        }
        assert!(
            expected_hops.contains(&hops),
            "{label}: expected {expected_hops:?} Up presses to reach the top visual row, got {hops}"
        );

        // From the top visual row, Up climbs onto the bottom label row.
        modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            modal.metadata_focus,
            Some(MetadataField::Failure),
            "{label}"
        );
    }
}

#[test]
fn tab_and_up_never_focus_rows_a_tight_render_dropped() {
    // Same 2-row content area as the tight-height render test: the label rows are dropped,
    // so keyboard entry must not park focus (and hide the caret) on an invisible row.
    let area = Rect::new(0, 0, 100, 22);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("tight");
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("short-but-valid area should render the modal");

    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        modal.metadata_focus, None,
        "Tab must not focus a row the render dropped"
    );
    modal.handle_key(&key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        modal.metadata_focus, None,
        "Up must not focus a row the render dropped"
    );
}

#[test]
fn tab_and_up_without_supplied_metadata_stay_in_the_composer() {
    for code in [KeyCode::Tab, KeyCode::Up] {
        let mut modal = open_with_text("no labels");

        assert_eq!(
            modal.handle_key(&key(code, KeyModifiers::NONE)),
            FeedbackModalOutcome::Changed,
            "{code:?}"
        );
        assert_eq!(modal.metadata_focus, None, "{code:?}");
        // Nothing was focusable, so Enter still submits from the composer, never opens a picker.
        assert_eq!(
            modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            FeedbackModalOutcome::Submit,
            "{code:?}"
        );
        assert!(!modal.enum_picker_open(), "{code:?}");
    }
}

#[test]
fn clicking_a_label_row_focuses_it() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("click a row");
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");

    // Click the middle row where it rendered; the stored rect makes the whole row clickable.
    let rendered = buffer_text(&buf);
    let row = rendered
        .lines()
        .position(|line| line.contains("Task: Debug"))
        .expect("Task row renders") as u16;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 40,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(modal.handle_mouse(&click), FeedbackModalOutcome::Changed);
    assert_eq!(modal.metadata_focus, Some(MetadataField::Task));

    // The clicked row is focused: Enter opens its picker instead of submitting.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert!(modal.enum_picker_open());
}

#[test]
fn typing_while_the_metadata_row_is_focused_resumes_the_draft() {
    let mut modal = open_with_all_metadata("draft");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));

    // A printable key belongs to the composer: focus returns and the character lands in the draft.
    modal.handle_key(&key(KeyCode::Char('!'), KeyModifiers::NONE));

    assert_eq!(modal.text(), "draft!");
    assert_eq!(
        modal.metadata().r#type,
        Some(FeedbackType::Bug),
        "typing must not cycle an enum"
    );
}

#[test]
fn esc_from_the_metadata_row_returns_to_the_composer_without_cancelling() {
    let mut modal = open_with_all_metadata("keep me");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed,
        "the first Esc only drops the row focus"
    );
    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Cancel,
        "the next Esc reaches the chrome and cancels"
    );
}

#[test]
fn clicking_the_composer_returns_focus_from_the_metadata_row() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("click me");
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(modal.metadata_focus, Some(MetadataField::Type));

    // Click the draft text inside the composer, below the label rows.
    let rendered = buffer_text(&buf);
    let row = rendered
        .lines()
        .position(|line| line.contains("click me"))
        .expect("draft renders") as u16;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 40,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(modal.handle_mouse(&click), FeedbackModalOutcome::Changed);

    // Focus is back on the composer: Enter submits instead of editing a hidden-focus label.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Submit
    );
    assert_eq!(
        modal.metadata().r#type,
        Some(FeedbackType::Bug),
        "the click must not edit a label"
    );
}

// -- Enum variant picker: open from a chip, commit, filter, and back out --

#[test]
fn enter_on_a_focused_chip_opens_the_picker_and_commits_a_later_variant() {
    let mut modal = open_with_all_metadata("picker");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!modal.enum_picker_open());

    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed,
        "Enter on a chip opens the picker instead of submitting"
    );
    assert!(modal.enum_picker_open());

    // The picker opens highlighting the current value (Bug); two Downs land on a later variant.
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed,
        "committing a variant never submits the report"
    );
    assert!(!modal.enum_picker_open());
    assert_eq!(
        modal.metadata().r#type,
        Some(FeedbackType::MissingCapability)
    );

    // Focus returned to the same chip: Enter reopens its picker rather than submitting.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert!(modal.enum_picker_open());
}

#[test]
fn picker_type_to_filter_commits_the_matching_variant() {
    let mut modal = open_with_all_metadata("filter");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    // Down twice walks from the first row (Type) to the Failure row; Left/Right cycle values, not move.
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(modal.enum_picker_open());

    for ch in "loop".chars() {
        modal.handle_key(&key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );

    assert!(!modal.enum_picker_open());
    assert_eq!(
        modal.metadata().failure_mode,
        Some(FeedbackFailureMode::StuckInALoop)
    );
}

#[test]
fn picker_esc_returns_to_the_chip_without_changing_the_value() {
    let mut modal = open_with_all_metadata("cancel");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(modal.enum_picker_open());
    modal.handle_key(&key(KeyCode::Down, KeyModifiers::NONE));

    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed,
        "Esc only closes the picker, never the modal"
    );
    assert!(!modal.enum_picker_open());
    assert_eq!(
        modal.metadata().r#type,
        Some(FeedbackType::Bug),
        "backing out commits nothing"
    );

    // The label row is still focused: the next Esc drops focus, the one after cancels.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Cancel
    );
}

#[test]
fn open_picker_renders_the_variant_list_over_the_composer() {
    let mut modal = open_with_all_metadata("hidden draft");
    render_wide(&mut modal);
    modal.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let frame = modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");

    let rendered = buffer_text(&buf);
    for label in ["Bug", "Idea", "Missing capability"] {
        assert!(rendered.contains(label), "{rendered}");
    }
    assert!(
        !rendered.contains("hidden draft"),
        "the picker replaces the composer"
    );
    assert!(
        rendered.contains("Type: Bug"),
        "the label rows stay as context above the picker"
    );
    assert!(
        frame.cursor.is_none(),
        "the picker's search bar paints its own cursor cell"
    );
}

/// The label row is editable, so it must not render in the disabled gray the old static summary used.
#[test]
fn metadata_row_is_not_rendered_disabled_gray() {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_all_metadata("styled");
    // The default theme is unquantized, so color identity is stable without a terminal.
    let theme = Theme::default();
    modal
        .render(&mut buf, area, &theme, false)
        .expect("wide area should render the modal");

    let rendered = buffer_text(&buf);
    let row = row_of(&rendered, "Type: Bug");
    let line = rendered.lines().nth(row).unwrap();
    // Byte offsets overshoot columns once the border's box-drawing chars appear; count cells instead.
    let col = line
        .find("Type: Bug")
        .and_then(|idx| line.get(..idx))
        .map(|s| s.chars().count() as u16)
        .unwrap_or_else(|| panic!("Type: Bug must render"));
    assert_ne!(
        buf.cell((col, row as u16)).unwrap().style().fg,
        Some(theme.gray)
    );
}

#[test]
fn loading_a_draft_requires_discard_for_fresh_write_and_replaces_attachments() {
    let mut modal = open_with_text("fresh composition");
    modal
        .composer_mut()
        .insert_image(test_image(1))
        .expect("fresh attachment");
    modal.window.active_tab = FeedbackTab::Drafts.index();
    modal.drafts = DraftsState::Browse {
        rows: vec![draft("stored", "stored text", FeedbackTaxonomy::default())],
        selected_id: Some("stored".to_owned().into()),
        query: LineEditor::default(),
        search_focused: false,
        error: None,
    };

    modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(modal.discard_confirm, Some("stored".to_owned().into()));
    assert!(modal.take_pending_request().is_none());
    assert!(modal.text().contains("fresh composition"));
    assert_eq!(modal.image_count(), 1);

    modal.handle_key(&key(KeyCode::Char('y'), KeyModifiers::NONE));
    let FeedbackDraftRequest::Load(load) = modal.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };
    modal.apply_draft_load(
        &load,
        draft("stored", "stored text", FeedbackTaxonomy::default()),
    );

    assert_eq!(modal.active_tab(), FeedbackTab::Write);
    assert_eq!(modal.text(), "stored text");
    assert_eq!(modal.image_count(), 0);
}

/// A bare open peeks at Drafts; it stays there only when the list comes back nonempty.
#[test]
fn bare_open_lands_on_write_unless_the_draft_list_has_rows() {
    let open = |result: Result<Vec<FeedbackDraft>, String>| {
        let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
        modal.start_open_draft_list();
        assert_eq!(modal.active_tab(), FeedbackTab::Drafts);
        let FeedbackDraftRequest::List {
            modal_id,
            generation,
        } = modal.take_pending_request().expect("list request")
        else {
            panic!("expected draft list");
        };
        match result {
            Ok(rows) => modal.apply_draft_list(modal_id, generation, rows),
            Err(error) => modal.fail_draft_list(modal_id, generation, error),
        }
        modal.active_tab()
    };

    assert_eq!(open(Err("disabled".to_owned())), FeedbackTab::Write);
    assert_eq!(open(Ok(Vec::new())), FeedbackTab::Write);
    assert_eq!(
        open(Ok(vec![draft(
            "stored",
            "stored text",
            FeedbackTaxonomy::default()
        )])),
        FeedbackTab::Drafts
    );
}

/// Leaving the peek by keyboard or mouse ends it: a late nonempty list lands without moving the user.
#[test]
fn bare_open_does_not_yank_the_user_back_to_drafts_after_they_left() {
    fn stays_on_write_after(label: &str, leave_peek: impl FnOnce(&mut FeedbackModalState)) {
        let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
        modal.start_open_draft_list();
        let FeedbackDraftRequest::List {
            modal_id,
            generation,
        } = modal.take_pending_request().expect("list request")
        else {
            panic!("expected draft list");
        };

        leave_peek(&mut modal);
        assert_eq!(modal.active_tab(), FeedbackTab::Write, "{label}");
        for c in ['h', 'e', 'l'] {
            modal.handle_key(&key(KeyCode::Char(c), KeyModifiers::NONE));
        }

        modal.apply_draft_list(
            modal_id,
            generation,
            vec![draft("stored", "stored text", FeedbackTaxonomy::default())],
        );

        assert_eq!(modal.active_tab(), FeedbackTab::Write, "{label}");
        assert_eq!(modal.text(), "hel", "{label}");
        assert!(
            matches!(&modal.drafts, DraftsState::Browse { rows, .. } if rows.len() == 1),
            "{label}: the late list must land, not be dropped"
        );
    }

    stays_on_write_after("ctrl-tab", |modal| {
        modal.handle_key(&key(KeyCode::Tab, KeyModifiers::CONTROL));
    });
    stays_on_write_after("click", |modal| {
        render_to_text(modal);
        let write_tab = modal
            .window
            .tab_rects
            .get(FeedbackTab::Write.index())
            .copied()
            .flatten()
            .expect("Write tab rect");
        modal.handle_mouse(&left_click(write_tab.x, write_tab.y));
    });
}

#[test]
fn loading_a_draft_starts_a_new_paste_composition() {
    let mut modal = open_with_text("draft A");
    let original_composition = modal.composition_id();
    modal.note_paste_probe_started();
    modal.deferred_submit = true;
    modal.start_draft_load("draft-b".to_owned().into());
    let FeedbackDraftRequest::Load(load) = modal.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };

    modal.apply_draft_load(
        &load,
        draft("draft-b", "draft B", FeedbackTaxonomy::default()),
    );

    assert!(!modal.matches_composition(original_composition));
    assert_eq!(modal.paste_probes_in_flight, 0);
    assert!(!modal.deferred_submit);
    assert_eq!(modal.text(), "draft B");
}

#[test]
fn draft_load_is_invalidated_by_write_edits_or_submit_unknown() {
    let mut modal = open_with_text("prefilled write");
    modal.start_external_draft_load("stored".to_owned().into());
    let FeedbackDraftRequest::Load(load) = modal.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };

    modal.handle_key(&key(KeyCode::Char('!'), KeyModifiers::NONE));
    modal.apply_draft_load(
        &load,
        draft("stored", "stale text", FeedbackTaxonomy::default()),
    );

    assert_eq!(modal.text(), "prefilled write!");

    // An unknown-outcome send also drops the pending load, so the stale row cannot clear its banner.
    let mut unknown = FeedbackModalState::new(OpenFeedbackModal::default());
    unknown.start_draft_load("stored".to_owned().into());
    let FeedbackDraftRequest::Load(load) = unknown.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };

    unknown.enter_write();
    unknown.mark_draft_submit_unknown();
    unknown.apply_draft_load(
        &load,
        draft("stored", "stale text", FeedbackTaxonomy::default()),
    );

    assert_eq!(unknown.text(), "");
    assert!(unknown.is_draft_submit_unknown());
}

#[test]
fn terminal_draft_outcomes_leave_trace_step_for_visible_warning() {
    for (terminal, warning) in [
        (
            DraftSubmitTerminal::CleanupFailed,
            "stored draft could not be deleted",
        ),
        (
            DraftSubmitTerminal::OutcomeUnknown,
            "remote outcome is unknown",
        ),
    ] {
        let mut modal = open_with_text("stored feedback");
        modal.begin_trace_step();
        match terminal {
            DraftSubmitTerminal::CleanupFailed => modal.mark_draft_cleanup_failed(),
            DraftSubmitTerminal::OutcomeUnknown => {
                let _ = modal.mark_draft_submit_unknown();
            }
        }

        assert!(!modal.in_trace_step());
        let rendered = render_to_text(&mut modal);
        assert!(rendered.contains(warning), "{rendered}");
    }
}

#[test]
fn mark_draft_submit_unknown_queues_an_update_only_for_a_loaded_draft() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    modal.start_draft_load("draft-1".to_owned().into());
    let FeedbackDraftRequest::Load(load) = modal.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };
    modal.apply_draft_load(
        &load,
        draft("draft-1", "stored title", FeedbackTaxonomy::default()),
    );
    modal.composer_mut().set_text("edited composer");

    let copied = modal.mark_draft_submit_unknown();
    assert_eq!(copied.as_deref(), Some("stored title\n\nedited composer"));
    let FeedbackDraftRequest::Update(update) =
        modal.take_pending_request().expect("update request")
    else {
        panic!("expected draft update");
    };
    assert_eq!(update.draft_id.as_str(), "draft-1");
    assert_eq!(update.details, "edited composer");
    assert_eq!(update.title, "stored title");
    assert_eq!(update.r#type, FeedbackType::Bug);
    // Only the persistence claim is pinned, not the rest of the banner wording.
    let pending = modal.error_text().expect("unknown-outcome banner");
    assert!(
        !pending.contains("was saved"),
        "no persistence claim before the update completes"
    );
    modal.apply_draft_update_complete(&update, None);
    assert!(
        modal
            .error_text()
            .is_some_and(|text| text.contains("was saved")),
        "a successful update may then claim the draft was saved"
    );
    modal.apply_draft_update_complete(&update, Some("store unavailable"));
    assert!(
        modal
            .error_text()
            .is_some_and(|text| text.contains("could not be saved")),
        "a failed update retracts the persistence claim"
    );
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(
        modal.handle_key(&key(KeyCode::Char('x'), KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(modal.text(), "edited composer");
    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Cancel
    );

    // Without a draft id there is no row to write back to; only the clipboard copy happens.
    let mut free = open_with_text("free composition");
    assert_eq!(
        free.mark_draft_submit_unknown().as_deref(),
        Some("free composition")
    );
    assert!(free.take_pending_request().is_none());
    assert!(
        free.error_text()
            .is_some_and(|text| !text.contains("saved")),
        "no store row, so no store claim"
    );
    assert_eq!(
        free.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );
    assert_eq!(
        free.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Cancel
    );
}

#[test]
fn delete_completion_settles_by_delete_token_after_draft_refresh() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    modal.window.active_tab = FeedbackTab::Drafts.index();
    modal.drafts = DraftsState::Browse {
        rows: vec![draft(
            "delete-me",
            "stored text",
            FeedbackTaxonomy::default(),
        )],
        selected_id: Some("delete-me".to_owned().into()),
        query: LineEditor::default(),
        search_focused: false,
        error: None,
    };
    modal.handle_key(&key(KeyCode::Char('d'), KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Char('y'), KeyModifiers::NONE));
    let FeedbackDraftRequest::Delete(delete) =
        modal.take_pending_request().expect("delete request")
    else {
        panic!("expected draft delete");
    };

    let stale = FeedbackDraftDelete {
        token: FeedbackDraftDeleteToken(delete.token.0.wrapping_add(1)),
        ..delete.clone()
    };
    modal.apply_draft_delete(&stale);
    assert_eq!(modal.draft_delete.as_ref(), Some(&delete));

    modal.refresh_drafts();
    modal.apply_draft_delete(&delete);

    assert!(modal.draft_delete.is_none());
    assert!(modal.delete_confirm.is_none());
}

#[test]
fn mouse_tab_change_is_blocked_while_delete_is_pending() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    modal.window.active_tab = FeedbackTab::Drafts.index();
    modal.drafts = DraftsState::Browse {
        rows: vec![draft(
            "delete-me",
            "stored text",
            FeedbackTaxonomy::default(),
        )],
        selected_id: Some("delete-me".to_owned().into()),
        query: LineEditor::default(),
        search_focused: false,
        error: None,
    };
    render_to_text(&mut modal);
    modal.handle_key(&key(KeyCode::Char('d'), KeyModifiers::NONE));
    modal.handle_key(&key(KeyCode::Char('y'), KeyModifiers::NONE));
    let write_tab = modal
        .window
        .tab_rects
        .get(FeedbackTab::Write.index())
        .copied()
        .flatten()
        .expect("Write tab rect");

    modal.handle_mouse(&MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: write_tab.x,
        row: write_tab.y,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(modal.active_tab(), FeedbackTab::Drafts);
    assert!(modal.draft_delete.is_some());
}

#[test]
fn draft_search_bar_is_always_visible_and_click_focuses_it() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    modal.window.active_tab = FeedbackTab::Drafts.index();
    modal.drafts = DraftsState::Browse {
        rows: vec![draft("stored", "stored text", FeedbackTaxonomy::default())],
        selected_id: Some("stored".to_owned().into()),
        query: LineEditor::default(),
        search_focused: false,
        error: None,
    };

    let rendered = render_to_text(&mut modal);
    assert!(rendered.contains("/ to search"), "{rendered}");
    let search_area = modal.draft_search_area.expect("search hit area");
    modal.handle_mouse(&left_click(search_area.x, search_area.y));

    assert!(matches!(
        modal.drafts,
        DraftsState::Browse {
            search_focused: true,
            ..
        }
    ));
    assert!(modal.take_pending_request().is_none());
}

#[test]
fn enter_from_focused_search_opens_filtered_selection() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    modal.window.active_tab = FeedbackTab::Drafts.index();
    let mut query = LineEditor::default();
    assert_eq!(
        query.insert_paste("second"),
        crate::input::line_editor::LineEditOutcome::TextChanged
    );
    modal.drafts = DraftsState::Browse {
        rows: vec![
            draft("first", "first result", FeedbackTaxonomy::default()),
            draft("second", "second result", FeedbackTaxonomy::default()),
        ],
        selected_id: Some("first".to_owned().into()),
        query,
        search_focused: true,
        error: None,
    };
    modal.normalize_draft_selection();

    modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));

    let FeedbackDraftRequest::Load(load) = modal.take_pending_request().expect("load request")
    else {
        panic!("expected draft load");
    };
    assert_eq!(load.draft_id.as_str(), "second");
}

#[test]
fn double_click_on_rendered_draft_row_starts_same_load_as_enter() {
    let make_modal = |open: OpenFeedbackModal| {
        let mut modal = FeedbackModalState::new(open);
        modal.window.active_tab = FeedbackTab::Drafts.index();
        modal.drafts = DraftsState::Browse {
            rows: vec![draft("stored", "stored text", FeedbackTaxonomy::default())],
            selected_id: Some("stored".to_owned().into()),
            query: LineEditor::default(),
            search_focused: false,
            error: None,
        };
        modal
    };
    let double_click_first_row = |modal: &mut FeedbackModalState| {
        render_to_text(modal);
        let Some((_, row)) = modal.draft_row_areas.first() else {
            panic!("expected a draft row: {:?}", modal.draft_row_areas);
        };
        let row = *row;
        modal.handle_mouse(&left_click(row.x, row.y));
        assert!(modal.take_pending_request().is_none());
        modal.handle_mouse(&left_click(row.x, row.y));
    };

    let mut keyboard = make_modal(OpenFeedbackModal::default());
    keyboard.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE));
    let keyboard_request = keyboard.take_pending_request();

    let mut mouse = make_modal(OpenFeedbackModal::default());
    double_click_first_row(&mut mouse);

    let FeedbackDraftRequest::Load(mouse_load) =
        mouse.take_pending_request().expect("mouse load request")
    else {
        panic!("expected mouse draft load");
    };
    let FeedbackDraftRequest::Load(keyboard_load) =
        keyboard_request.expect("keyboard load request")
    else {
        panic!("expected keyboard draft load");
    };
    assert_eq!(mouse_load.draft_id, keyboard_load.draft_id);
    assert_eq!(mouse_load.generation, keyboard_load.generation);

    // An unsaved Write composition gates the double-click behind the same discard confirm as Enter.
    let mut unsaved = make_modal(OpenFeedbackModal {
        text: Some("unsaved composition".to_owned()),
        ..Default::default()
    });
    double_click_first_row(&mut unsaved);

    assert_eq!(unsaved.discard_confirm, Some("stored".to_owned().into()));
    assert!(unsaved.take_pending_request().is_none());
}

#[test]
fn draft_viewport_keeps_last_selection_visible() {
    let mut modal = FeedbackModalState::new(OpenFeedbackModal::default());
    let rows: Vec<_> = (0..30)
        .map(|index| {
            draft(
                &format!("draft-{index}"),
                &format!("draft row {index}"),
                FeedbackTaxonomy::default(),
            )
        })
        .collect();
    modal.window.active_tab = FeedbackTab::Drafts.index();
    modal.drafts = DraftsState::Browse {
        selected_id: rows.first().map(|draft| draft.id.clone()),
        rows,
        query: LineEditor::default(),
        search_focused: false,
        error: None,
    };
    modal.handle_key(&key(KeyCode::Char('G'), KeyModifiers::NONE));

    let rendered = render_to_text(&mut modal);

    assert!(rendered.contains("draft row 29"), "{rendered}");
    assert!(!rendered.contains("draft row 0"), "{rendered}");
}

#[test]
fn agent_input_is_owned_without_mutating_main_prompt() {
    let mut agent = crate::app::agent_view::test_fixtures::make_agent();
    agent.prompt.set_text("main draft ");
    agent
        .prompt
        .insert_image(test_image(0))
        .expect("main image");
    let main_text = agent.prompt.text().to_string();
    let main_image_count = agent.prompt.images.len();
    let mut modal = open_with_text("modal draft");
    modal
        .composer_mut()
        .insert_image(test_image(0))
        .expect("modal image");
    agent.feedback_modal = Some(modal);
    let registry = crate::actions::ActionRegistry::defaults();

    let outcome = agent.handle_input(
        &crossterm::event::Event::Key(key(KeyCode::Char('!'), KeyModifiers::NONE)),
        &registry,
    );

    assert_eq!(agent.prompt.text(), main_text);
    assert_eq!(agent.prompt.images.len(), main_image_count);
    assert_eq!(
        agent.feedback_modal.as_ref().map(FeedbackModalState::text),
        Some("modal draft[Image #1] !")
    );
    assert!(matches!(
        outcome,
        crate::app::app_view::InputOutcome::Changed
    ));

    let _ = agent.handle_input(
        &crossterm::event::Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)),
        &registry,
    );
    assert!(agent.feedback_modal.is_none());
    assert_eq!(agent.prompt.text(), main_text);
    assert_eq!(agent.prompt.images.len(), main_image_count);
}

// -- Trace step: kind copy, choices, Esc, and step transitions --

fn render_to_text(modal: &mut FeedbackModalState) -> String {
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("wide area should render the modal");
    buffer_text(&buf)
}

#[test]
fn trace_copy_follows_supplied_type_never_user_text() {
    // The report text names an idea; the supplied Bug type must still pick the bug copy.
    let mut modal = FeedbackModalState::new(OpenFeedbackModal {
        text: Some("an idea about the crash".to_string()),
        r#type: Some(FeedbackType::Bug),
        ..Default::default()
    });
    modal.begin_trace_step();
    let rendered = render_to_text(&mut modal);
    assert!(rendered.contains("debug this bug"), "{rendered}");
    assert!(
        !rendered.contains("an idea about the crash"),
        "the trace step hides the composer draft"
    );

    // An absent type uses the neutral copy even when the text screams bug.
    let mut modal = FeedbackModalState::new(OpenFeedbackModal {
        text: Some("this is clearly a bug".to_string()),
        r#type: None,
        ..Default::default()
    });
    modal.begin_trace_step();
    let rendered = render_to_text(&mut modal);
    assert!(rendered.contains("to your feedback?"), "{rendered}");
    assert!(
        !rendered.contains("debug this bug"),
        "type must never be inferred from user-authored text"
    );
}

#[test]
fn trace_step_renders_every_choice_at_eighty_by_twenty_four_and_enter_decides_the_default_once() {
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    let mut modal = open_with_text("report");
    modal.begin_trace_step();

    modal
        .render(&mut buf, area, &Theme::current(), false)
        .expect("supported terminal should render the trace step");
    let rendered = buffer_text(&buf);
    // The one-shot consent scope may wrap at this width, so check both halves.
    for assurance in ["Nothing", "is turned on for future sessions."] {
        assert!(rendered.contains(assurance), "{rendered}");
    }
    for choice in FeedbackTraceChoice::ALL {
        assert!(rendered.contains(choice.label()), "{rendered}");
    }

    // Default highlight is FeedbackOnly so Enter cannot grant an upload.
    assert_eq!(
        modal.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
        FeedbackModalOutcome::Submit
    );
    assert_eq!(
        modal.take_decided_trace_choice(),
        Some(FeedbackTraceChoice::FeedbackOnly)
    );
    // Consumption is one-shot: a replayed submit finds no decided choice to send.
    assert_eq!(modal.take_decided_trace_choice(), None);
}

#[test]
fn trace_step_esc_returns_to_write_without_sending() {
    let mut modal = open_with_text("keep me");
    modal.begin_trace_step();

    assert_eq!(
        modal.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
        FeedbackModalOutcome::Changed
    );

    assert!(!modal.in_trace_step());
    assert_eq!(modal.text(), "keep me", "the draft survives backing out");
    assert!(modal.take_decided_trace_choice().is_none());
}

#[test]
fn constructor_keeps_disk_backed_image_for_async_rehydration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_copy = dir.path().join("feedback.png");
    std::fs::write(&session_copy, b"fake").expect("write session copy");
    let mut image = test_image(1);
    image.encoded_bytes = None;
    image.session_image_path = Some(session_copy.clone());

    let modal = FeedbackModalState::new(OpenFeedbackModal {
        images: vec![image].into(),
        ..Default::default()
    });

    assert!(
        session_copy.exists(),
        "the input reducer must not read or unlink it"
    );
    assert_eq!(modal.image_rehydration_requests().len(), 1);
    drop(modal);
    assert!(
        session_copy.exists(),
        "closing before rehydration must preserve the only persisted copy"
    );
}

#[test]
fn successful_rehydration_installs_bytes_before_unlinking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_copy = dir.path().join("feedback.png");
    std::fs::write(&session_copy, b"fake").expect("write session copy");
    let mut image = test_image(1);
    image.encoded_bytes = None;
    image.session_image_path = Some(session_copy.clone());
    let identity = image.preview.identity();
    let mut modal = FeedbackModalState::new(OpenFeedbackModal {
        images: vec![image].into(),
        ..Default::default()
    });

    modal.apply_rehydrated_image(identity, Ok(b"fake".to_vec()));

    assert!(!session_copy.exists());
    assert!(
        modal
            .composer
            .images
            .first()
            .is_some_and(|img| img.encoded_bytes.is_some())
    );
}

#[test]
fn failed_rehydration_keeps_the_only_persisted_copy_and_drops_the_chip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_copy = dir.path().join("feedback.png");
    std::fs::write(&session_copy, b"fake").expect("write session copy");
    let mut image = test_image(1);
    image.encoded_bytes = None;
    image.session_image_path = Some(session_copy.clone());
    let identity = image.preview.identity();
    let mut modal = FeedbackModalState::new(OpenFeedbackModal {
        images: vec![image].into(),
        ..Default::default()
    });

    modal.apply_rehydrated_image(identity, Err("read failed".to_string()));

    assert!(session_copy.exists());
    assert_eq!(modal.image_count(), 0);
    assert!(!modal.text().contains("[Image #"));
}

#[test]
fn cancel_deletes_staged_temp_files_of_owned_images() {
    let dir = tempfile::tempdir().expect("tempdir");
    let staged = dir.path().join("staged.png");
    std::fs::write(&staged, b"fake").expect("write staged");
    let mut image = test_image(1);
    image.staged_temp_path = Some(staged.clone());

    let mut agent = crate::app::agent_view::test_fixtures::make_agent();
    let modal = FeedbackModalState::new(OpenFeedbackModal {
        images: vec![image].into(),
        ..Default::default()
    });
    assert_eq!(modal.image_count(), 1);
    assert!(staged.exists());
    agent.feedback_modal = Some(modal);

    let registry = crate::actions::ActionRegistry::defaults();
    let _ = agent.handle_input(
        &crossterm::event::Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)),
        &registry,
    );

    assert!(agent.feedback_modal.is_none());
    assert!(!staged.exists(), "cancel must delete the staged temp file");
}
