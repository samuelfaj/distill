use fs2::FileExt as _;

use super::*;
use crate::{FeedbackFailureMode, FeedbackTaskCategory, FeedbackType};

fn input(details: &str) -> FeedbackDraftInput {
    FeedbackDraftInput {
        title: "Draft title".to_owned(),
        details: details.to_owned(),
        area: None,
        r#type: FeedbackType::Bug,
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode: Some(FeedbackFailureMode::Hallucinated),
    }
}

fn draft(id: impl Into<String>, details: impl Into<String>, revision: u64) -> FeedbackDraft {
    FeedbackDraft {
        id: id.into().into(),
        title: "Draft title".to_owned(),
        details: details.into(),
        area: None,
        r#type: Some(FeedbackType::Bug),
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode: None,
        created_at: 1,
        revision,
    }
}

fn document(drafts: Vec<FeedbackDraft>) -> FeedbackDraftDocument {
    FeedbackDraftDocument {
        schema_version: SCHEMA_VERSION,
        drafts,
    }
}

fn document_with_serialized_len(target: usize) -> FeedbackDraftDocument {
    let draft_count = MAX_DOCUMENT_BYTES.div_ceil(MAX_TEXT_BYTES);
    let mut document = document(
        (0..draft_count)
            .map(|index| draft(format!("draft-{index}"), "x", 1))
            .collect(),
    );
    let base = serde_json::to_vec_pretty(&document).unwrap().len();
    let mut remaining = target.checked_sub(base).unwrap();
    for draft in &mut document.drafts {
        let added = remaining.min(MAX_TEXT_BYTES - draft.details.len());
        draft.details.push_str(&"x".repeat(added));
        remaining -= added;
    }
    assert_eq!(remaining, 0);
    assert_eq!(serde_json::to_vec_pretty(&document).unwrap().len(), target);
    document
}

fn write_document(path: &Path, document: &FeedbackDraftDocument) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(document).unwrap();
    bytes.push(b'\n');
    std::fs::write(path, &bytes).unwrap();
    bytes
}

#[test]
fn append_list_get_and_delete_roundtrip() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());

    let first = store.append(input("first")).unwrap();
    let second = store.append(input("second")).unwrap();

    assert_eq!(first.revision, 1);
    assert_eq!(
        uuid::Uuid::parse_str(first.id.as_str())
            .unwrap()
            .get_version(),
        Some(uuid::Version::SortRand)
    );
    assert!(first.created_at > 0);
    assert_eq!(store.list().unwrap(), [first.clone(), second.clone()]);
    assert_eq!(store.get(&first.id).unwrap(), Some(first.clone()));
    assert_eq!(store.delete(&first.id).unwrap(), DeleteOutcome::Deleted);
    assert_eq!(store.list().unwrap(), std::slice::from_ref(&second));

    let before = std::fs::read(session.path().join(FEEDBACK_DRAFTS_FILENAME)).unwrap();
    assert_eq!(store.delete(&first.id).unwrap(), DeleteOutcome::NotFound);
    assert_eq!(
        std::fs::read(session.path().join(FEEDBACK_DRAFTS_FILENAME)).unwrap(),
        before
    );
    assert_eq!(store.delete(&second.id).unwrap(), DeleteOutcome::Deleted);
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn update_from_input_replaces_existing_and_miss_does_not_append() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let first = store
        .append_predraft("Todo list", "todos are chopped")
        .unwrap();
    assert_eq!(first.r#type, None);
    let missing: FeedbackDraftId = "missing-draft".to_owned().into();

    assert_eq!(
        store
            .update_from_input(&missing, input("new body"))
            .unwrap(),
        UpdateOutcome::NotFound
    );
    assert_eq!(store.list().unwrap().len(), 1);

    assert_eq!(
        store
            .update_from_input(&first.id, input("structured body"))
            .unwrap(),
        UpdateOutcome::Updated
    );
    let updated = store.get(&first.id).unwrap().expect("draft still present");
    assert_eq!(updated.title, "Draft title");
    assert_eq!(updated.details, "structured body");
    assert_eq!(updated.r#type, Some(FeedbackType::Bug));
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn missing_file_is_empty_but_invalid_input_writes_nothing() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());

    assert!(store.list().unwrap().is_empty());
    assert!(matches!(
        store.append(input(" \n ")),
        Err(FeedbackStoreError::BlankDetails)
    ));
    let mut blank_title = input("details");
    blank_title.title = " \n ".to_owned();
    assert!(matches!(
        store.append(blank_title),
        Err(FeedbackStoreError::BlankTitle)
    ));
    assert!(!session.path().join(FEEDBACK_DRAFTS_FILENAME).exists());
}

#[test]
fn old_stored_wires_load_without_duplicate_text_or_required_type() {
    let session = tempfile::tempdir().unwrap();
    let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);
    std::fs::write(
        &path,
        br#"{
  "schema_version": 1,
  "drafts": [{
    "id": "legacy",
    "text": "What happened:\n- The answer was made up.",
    "task_category": "debug",
    "failure_mode": "wrong_or_made_up",
    "created_at": 1,
    "revision": 1
  }]
}
"#,
    )
    .unwrap();
    let store = FeedbackDraftStore::new(session.path());

    let drafts = store.list().unwrap();
    let [draft] = drafts.as_slice() else {
        panic!("expected one draft: {drafts:?}");
    };
    assert_eq!(draft.title, "What happened:");
    assert_eq!(draft.details, "- The answer was made up.");
    assert_eq!(draft.r#type, None);
    assert_eq!(draft.failure_mode, Some(FeedbackFailureMode::Hallucinated));

    store.append(input("new draft")).unwrap();
    let rewritten = std::fs::read_to_string(path).unwrap();
    assert!(!rewritten.contains("wrong_or_made_up"));
    assert!(rewritten.contains("hallucinated"));
    assert!(!rewritten.contains("\"text\""));
}

#[test]
fn legacy_single_line_and_long_first_line_preserve_all_text_once() {
    let single = split_legacy_text("Only one line".to_owned());
    assert_eq!(
        single,
        ("Feedback draft".to_owned(), "Only one line".to_owned())
    );

    let first_line = "x".repeat(100);
    let text = format!("{first_line}\nsecond line");
    let (title, details) = split_legacy_text(text.clone());
    assert_eq!(title, "x".repeat(80));
    assert_eq!(
        format!("{title}{}", details.replace('\n', "")),
        text.replace('\n', "")
    );
    assert_eq!(details, format!("{}\nsecond line", "x".repeat(20)));
}

#[test]
fn read_regular_capped_reads_at_cap_and_rejects_over_cap_without_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    std::fs::write(&path, [7u8; 16]).unwrap();

    assert_eq!(read_regular_capped(&path, 16).unwrap(), [7u8; 16]);
    assert_eq!(
        read_regular_capped(&path, 15).unwrap_err().kind(),
        ErrorKind::FileTooLarge
    );
}

#[test]
fn corrupt_and_unsupported_documents_return_exact_errors_without_replacement() {
    let session = tempfile::tempdir().unwrap();
    let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);

    std::fs::write(&path, b"{").unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        FeedbackDraftStore::new(session.path()).list(),
        Err(FeedbackStoreError::Decode {
            path: error_path,
            ..
        }) if error_path == path
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    std::fs::write(&path, br#"{"schema_version":2,"drafts":[]}"#).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        FeedbackDraftStore::new(session.path()).list(),
        Err(FeedbackStoreError::UnsupportedSchema { version: 2 })
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn symlink_and_non_file_paths_return_typed_errors() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let data_path = session.path().join(FEEDBACK_DRAFTS_FILENAME);
    #[cfg(unix)]
    {
        let target = session.path().join("target");
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, &data_path).unwrap();
        assert!(matches!(
            store.list(),
            Err(FeedbackStoreError::SymlinkPath { path }) if path == data_path
        ));
        std::fs::remove_file(&data_path).unwrap();
    }

    std::fs::create_dir(&data_path).unwrap();
    assert!(matches!(
        store.list(),
        Err(FeedbackStoreError::NonFilePath { path }) if path == data_path
    ));
    std::fs::remove_dir(&data_path).unwrap();

    // The lock is acquired (and its file created) before the document is opened.
    let lock_path = session.path().join(FEEDBACK_DRAFTS_LOCK_FILENAME);
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();
    assert!(matches!(
        store.list(),
        Err(FeedbackStoreError::NonFilePath { path }) if path == lock_path
    ));
}

#[test]
fn contention_returns_busy_and_releases_after_unlock() {
    let session = tempfile::tempdir().unwrap();
    let lock_path = session.path().join(FEEDBACK_DRAFTS_LOCK_FILENAME);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let store = FeedbackDraftStore::new(session.path());

    assert!(matches!(store.list(), Err(FeedbackStoreError::Busy)));
    lock.unlock().unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn contention_released_within_retry_budget_succeeds() {
    let session = tempfile::tempdir().unwrap();
    let lock_path = session.path().join(FEEDBACK_DRAFTS_LOCK_FILENAME);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let store = FeedbackDraftStore::new(session.path());

    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        lock.unlock().unwrap();
    });
    assert!(store.list().unwrap().is_empty());
    holder.join().unwrap();
}

#[test]
fn append_omits_absent_failure_mode_and_cleans_temporary_file() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let appended = store
        .append(FeedbackDraftInput {
            title: "Plan idea".to_owned(),
            details: "no failure".to_owned(),
            area: Some("pager".to_owned()),
            r#type: FeedbackType::Idea,
            task_category: Some(FeedbackTaskCategory::Plan),
            failure_mode: None,
        })
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(session.path().join(FEEDBACK_DRAFTS_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "schema_version": 1,
            "drafts": [{
                "id": appended.id.as_str(),
                "title": "Plan idea",
                "details": "no failure",
                "area": "pager",
                "type": "idea",
                "task_category": "plan",
                "created_at": appended.created_at,
                "revision": 1,
            }],
        })
    );
    assert!(
        std::fs::read_dir(session.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(FEEDBACK_DRAFTS_TEMP_PREFIX))
    );
}

fn oversized_text_field(error: &FeedbackStoreError) -> Option<(&'static str, usize, usize)> {
    match error {
        FeedbackStoreError::TitleTooLarge { observed, cap } => Some(("title", *observed, *cap)),
        FeedbackStoreError::DetailsTooLarge { observed, cap } => Some(("details", *observed, *cap)),
        FeedbackStoreError::AreaTooLarge { observed, cap } => Some(("area", *observed, *cap)),
        _ => None,
    }
}

/// One labelled setter for a byte-capped text field of the draft input.
type SetTextField = (&'static str, fn(&mut FeedbackDraftInput, String));

#[test]
fn text_caps_accept_boundary_and_reject_over_without_replacement() {
    let cases: [SetTextField; 3] = [
        ("title", |input, text| input.title = text),
        ("details", |input, text| input.details = text),
        ("area", |input, text| input.area = Some(text)),
    ];

    for (field, set_field) in cases {
        let session = tempfile::tempdir().unwrap();
        let store = FeedbackDraftStore::new(session.path());
        let mut boundary = input("details");
        set_field(&mut boundary, "x".repeat(MAX_TEXT_BYTES));
        let mut over = input("details");
        set_field(&mut over, "x".repeat(MAX_TEXT_BYTES + 1));
        let expected = Some((field, MAX_TEXT_BYTES + 1, MAX_TEXT_BYTES));

        assert!(validate_feedback_draft_input(&boundary).is_ok(), "{field}");
        assert_eq!(
            oversized_text_field(&validate_feedback_draft_input(&over).unwrap_err()),
            expected
        );

        store.append(boundary).unwrap();
        let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            oversized_text_field(&store.append(over).unwrap_err()),
            expected
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "{field}");
    }
}

#[test]
fn send_validator_allows_titled_image_only_and_rejects_empty() {
    let blank = input("   ");
    assert!(matches!(
        validate_feedback_draft_send(&blank, false),
        Err(FeedbackStoreError::BlankDetails)
    ));
    assert!(validate_feedback_draft_send(&blank, true).is_ok());
}

#[test]
fn document_cap_accepts_boundary_and_rejects_over_without_replacement() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);
    let boundary = document_with_serialized_len(MAX_DOCUMENT_BYTES - 1);

    store.commit(&boundary).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        MAX_DOCUMENT_BYTES as u64
    );
    assert_eq!(store.list().unwrap(), boundary.drafts);

    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        store.commit(&document_with_serialized_len(MAX_DOCUMENT_BYTES)),
        Err(FeedbackStoreError::TooLarge { observed, cap })
            if observed == MAX_DOCUMENT_BYTES + 1 && cap == MAX_DOCUMENT_BYTES
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn draft_count_cap_accepts_boundary_and_rejects_over_without_replacement() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let document = document(
        (0..MAX_DRAFTS)
            .map(|index| draft(format!("draft-{index}"), "x", 1))
            .collect(),
    );
    store.commit(&document).unwrap();
    let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);
    let before = std::fs::read(&path).unwrap();
    let mut over_cap = document;
    over_cap.drafts.push(draft("one-too-many", "x", 1));

    assert!(matches!(
        store.commit(&over_cap),
        Err(FeedbackStoreError::DraftCapacityExceeded { observed, cap })
            if observed == MAX_DRAFTS + 1 && cap == MAX_DRAFTS
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn invalid_stored_documents_return_typed_errors_without_replacement() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let path = session.path().join(FEEDBACK_DRAFTS_FILENAME);

    let empty_id = document(vec![draft("", "text", 1)]);
    let before = write_document(&path, &empty_id);
    assert!(matches!(
        store.append(input("new draft")),
        Err(FeedbackStoreError::EmptyDraftId)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let duplicate_ids = document(vec![draft("same", "first", 1), draft("same", "second", 1)]);
    let before = write_document(&path, &duplicate_ids);
    assert!(matches!(
        store.append(input("new draft")),
        Err(FeedbackStoreError::DuplicateDraftId { id }) if id.as_str() == "same"
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let invalid_revision = document(vec![draft("draft", "text", 0)]);
    let before = write_document(&path, &invalid_revision);
    assert!(matches!(
        store.append(input("new draft")),
        Err(FeedbackStoreError::InvalidRevision { id }) if id.as_str() == "draft"
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let mut oversized_area = draft("oversized-area", "details", 1);
    oversized_area.area = Some("x".repeat(MAX_TEXT_BYTES + 1));
    let before = write_document(&path, &document(vec![oversized_area]));
    assert!(matches!(
        store.append(input("new draft")),
        Err(FeedbackStoreError::AreaTooLarge { observed, cap })
            if observed == MAX_TEXT_BYTES + 1 && cap == MAX_TEXT_BYTES
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
