use serde_json::{Value, json};

use super::*;

fn extension_for_mime(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

const POLICY: DraftImagePolicy = DraftImagePolicy {
    max_images: 2,
    max_image_bytes: 16,
    extension_for_mime,
};

fn image(fill: u8, len: usize, mime_type: &str) -> DraftImage {
    DraftImage {
        bytes: vec![fill; len],
        mime_type: mime_type.to_owned(),
    }
}

fn parts(images: &[DraftImage]) -> Vec<(&[u8], &str)> {
    images
        .iter()
        .map(|image| (image.bytes.as_slice(), image.mime_type.as_str()))
        .collect()
}

fn draft_id() -> FeedbackDraftId {
    FeedbackDraftId::from("draft-images".to_owned())
}

#[test]
fn write_then_read_round_trips_and_keeps_the_frozen_manifest_keys() {
    let session = tempfile::tempdir().unwrap();
    let id = draft_id();
    let images = [image(1, 16, "image/gif"), image(2, 3, "image/png")];

    write_draft_images(session.path(), &id, &images, POLICY).unwrap();

    assert_eq!(
        parts(&read_draft_images(session.path(), &id, POLICY)),
        parts(&images)
    );
    let manifest =
        std::fs::read(feedback_draft_images_dir(session.path(), &id).join(MANIFEST_FILENAME))
            .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&manifest).unwrap(),
        json!([
            {"fileName": "0.gif", "mimeType": "image/gif", "byteLen": 16},
            {"fileName": "1.png", "mimeType": "image/png", "byteLen": 3}
        ])
    );
}

#[test]
fn empty_or_rejected_input_writes_nothing() {
    let session = tempfile::tempdir().unwrap();
    let id = draft_id();
    write_draft_images(session.path(), &id, &[], POLICY).unwrap();
    let rejected = [
        vec![image(1, 0, "image/png")],
        vec![image(1, 4, "image/png"), image(1, 17, "image/png")],
        vec![image(1, 4, "image/webp")],
        vec![image(1, 4, "image/png"); 3],
    ];

    for images in rejected {
        let error = write_draft_images(session.path(), &id, &images, POLICY).unwrap_err();
        assert!(matches!(error, DraftImageError::Rejected { .. }), "{error}");
    }
    assert!(!session.path().join(FEEDBACK_DRAFT_IMAGES_DIRNAME).exists());
}

#[test]
fn planted_paths_are_refused_and_a_failed_write_leaves_nothing_behind() {
    fn extension_with_separator(mime_type: &str) -> Option<&'static str> {
        match mime_type {
            "image/png" => Some("png"),
            "image/broken" => Some("png/x"),
            _ => None,
        }
    }
    let session = tempfile::tempdir().unwrap();
    let id = draft_id();
    let dir = feedback_draft_images_dir(session.path(), &id);
    let images = [image(1, 4, "image/png"), image(2, 4, "image/broken")];
    let broken = DraftImagePolicy {
        extension_for_mime: extension_with_separator,
        ..POLICY
    };

    let error = write_draft_images(session.path(), &id, &images, broken).unwrap_err();
    assert!(
        matches!(&error, DraftImageError::Write { path, .. } if *path == dir.join("1.png/x")),
        "{error}"
    );
    assert!(!dir.exists());

    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("0.png"), [9u8; 4]).unwrap();
    let Some(one) = images.get(..1) else {
        panic!("expected at least one image: {images:?}");
    };
    let error = write_draft_images(session.path(), &id, one, POLICY).unwrap_err();
    assert!(
        matches!(&error, DraftImageError::Write { path, .. } if *path == dir),
        "{error}"
    );
    assert_eq!(std::fs::read(dir.join("0.png")).unwrap(), [9u8; 4]);
    assert!(!dir.join(MANIFEST_FILENAME).exists());

    #[cfg(unix)]
    {
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();
        let Some(one) = images.get(..1) else {
            panic!("expected at least one image: {images:?}");
        };
        let error = write_draft_images(session.path(), &id, one, POLICY).unwrap_err();
        assert!(
            matches!(&error, DraftImageError::Write { path, .. } if *path == dir),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}

#[test]
fn reader_caps_the_manifest_and_skips_entries_it_cannot_trust() {
    fn entry(name: &str, mime: &str, len: usize) -> Value {
        json!({"fileName": name, "mimeType": mime, "byteLen": len})
    }
    let session = tempfile::tempdir().unwrap();
    let id = draft_id();
    let dir = feedback_draft_images_dir(session.path(), &id);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    for (name, len) in [
        ("0.png", 1),
        ("1.png", 2),
        ("2.png", 3),
        ("big.png", 17),
        ("0.webp", 4),
        ("sub/0.png", 4),
        ("../escape.png", 4),
    ] {
        std::fs::write(dir.join(name), vec![7u8; len]).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("0.png"), dir.join("link.png")).unwrap();
    assert!(read_draft_images(session.path(), &id, POLICY).is_empty());

    let oversized = Value::Array(vec![entry("0.png", "image/png", 1); 2_000]).to_string();
    assert!(oversized.len() > MAX_MANIFEST_BYTES);
    type Case = (String, usize, Vec<DraftImage>);
    let cases: [Case; 5] = [
        ("[{".to_owned(), 2, vec![]),
        (r#"{"fileName":"0.png"}"#.to_owned(), 2, vec![]),
        (oversized, 2, vec![]),
        (
            json!([
                entry("0.png", "image/png", 1),
                entry("1.png", "image/png", 2),
                entry("2.png", "image/png", 3)
            ])
            .to_string(),
            2,
            vec![image(7, 1, "image/png"), image(7, 2, "image/png")],
        ),
        (
            json!([
                entry("../escape.png", "image/png", 4),
                entry("sub/0.png", "image/png", 4),
                entry("0.webp", "image/webp", 4),
                entry("1.png", "image/png", 3),
                entry("big.png", "image/png", 17),
                entry("link.png", "image/png", 1),
                entry("0.png", "image/png", 1)
            ])
            .to_string(),
            8,
            vec![image(7, 1, "image/png")],
        ),
    ];

    for (manifest, max_images, expected) in cases {
        std::fs::write(dir.join(MANIFEST_FILENAME), &manifest).unwrap();
        let policy = DraftImagePolicy {
            max_images,
            ..POLICY
        };
        assert_eq!(
            parts(&read_draft_images(session.path(), &id, policy)),
            parts(&expected),
            "{manifest:.40}"
        );
    }
}
