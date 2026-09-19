use super::*;

fn tar_names(bytes: &[u8]) -> Vec<String> {
    use flate2::read::GzDecoder;
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn session_archive_skips_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("chat_history.jsonl"), b"ok").unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, b"do-not-upload").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, dir.path().join("leak")).unwrap();

    let bytes = build_session_archive(dir.path(), "sid").unwrap();
    let names = tar_names(&bytes);
    assert!(
        names.iter().any(|n| n.ends_with("chat_history.jsonl")),
        "{names:?}"
    );
    assert!(
        names.iter().all(|n| !n.ends_with("leak")),
        "symlink must not be packed: {names:?}"
    );
}

#[test]
fn session_archive_skips_feedback_draft_artifacts_before_accounting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("trace.jsonl"), b"ok").unwrap();
    let artifacts = [
        crate::FEEDBACK_DRAFTS_FILENAME,
        crate::FEEDBACK_DRAFTS_LOCK_FILENAME,
        ".feedback_drafts.tmp-live",
    ];
    for name in artifacts {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }

    let caps = ArchiveCaps {
        archive_bytes: 64,
        file_bytes: 2,
    };
    let names = tar_names(
        &build_session_archive_with_caps(dir.path(), "sid", &caps)
            .expect("excluded drafts must not consume the archive budget"),
    );
    assert!(names.contains(&"sid/trace.jsonl".to_owned()));
    for name in artifacts {
        assert!(!names.contains(&format!("sid/{name}")), "{names:?}");
    }
}

#[test]
fn session_archive_skips_feedback_draft_images_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("trace.jsonl"), b"ok").unwrap();
    let draft_id = crate::FeedbackDraftId::from("01931111-aaaa-7bbb-8ccc-ddddeeeeffff".to_owned());
    let images = crate::feedback_draft_images_dir(dir.path(), &draft_id);
    std::fs::create_dir_all(&images).unwrap();
    std::fs::write(images.join("0.png"), b"\x89PNG screenshot bytes").unwrap();
    std::fs::write(
        images.join("metadata.json"),
        br#"[{"fileName":"0.png","mimeType":"image/png","byteLen":21}]"#,
    )
    .unwrap();

    let names = tar_names(&build_session_archive(dir.path(), "sid").unwrap());

    assert_eq!(names, vec!["sid/trace.jsonl"]);
}

#[cfg(unix)]
#[test]
fn session_archive_skips_feedback_draft_hardlink_under_another_name() {
    let dir = tempfile::tempdir().unwrap();
    let draft = dir.path().join(crate::FEEDBACK_DRAFTS_FILENAME);
    std::fs::write(&draft, b"private draft").unwrap();
    std::fs::hard_link(&draft, dir.path().join("innocent.json")).unwrap();
    std::fs::write(dir.path().join("trace.jsonl"), b"ok").unwrap();

    let names = tar_names(&build_session_archive(dir.path(), "sid").unwrap());

    assert_eq!(names, vec!["sid/trace.jsonl"]);
}

/// Hitting the total cap truncates the archive instead of failing it: a session just over the cap still uploads what was packed.
#[test]
fn archive_truncates_at_total_cap_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jsonl"), vec![b'x'; 8]).unwrap();
    std::fs::write(dir.path().join("b.jsonl"), vec![b'y'; 8]).unwrap();
    std::fs::write(dir.path().join("c.jsonl"), vec![b'z'; 8]).unwrap();

    let caps = ArchiveCaps {
        archive_bytes: 10,
        file_bytes: 10,
    };
    let bytes = build_session_archive_with_caps(dir.path(), "sid", &caps)
        .expect("capped archive must still build");
    let names = tar_names(&bytes);
    assert!(
        !names.is_empty() && names.len() < 3,
        "expected a truncated (but non-empty) archive: {names:?}"
    );
}

/// An archive where every file was skipped must fail, not upload an empty gzip while reporting success.
#[test]
fn archive_with_nothing_packed_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("huge.jsonl"), vec![b'x'; 32]).unwrap();

    let caps = ArchiveCaps {
        archive_bytes: 64,
        file_bytes: 8,
    };
    let err = build_session_archive_with_caps(dir.path(), "sid", &caps)
        .expect_err("all-skipped session must not produce an archive");
    assert!(err.to_string().contains("empty"), "{err}");
}
