use pretty_assertions::assert_eq;

use super::*;

#[test]
fn session_id_validates_path_component_rules() {
    let id = SessionId::new("session-abc_123").unwrap();
    assert_eq!(id.as_ref(), "session-abc_123");
    assert_eq!(id.to_string(), "session-abc_123");
    assert_eq!(
        SessionId::try_from("session-abc_123".to_owned()).unwrap(),
        id
    );

    let max_len = "a".repeat(MAX_SESSION_ID_BYTES);
    assert_eq!(SessionId::new(&*max_len).unwrap().as_ref(), max_len);

    for rejected in [
        String::new(),
        "a/b".to_owned(),
        "a\\b".to_owned(),
        ".".to_owned(),
        "..".to_owned(),
        "nul\0byte".to_owned(),
        "line\nbreak".to_owned(),
        "escape\u{1b}sequence".to_owned(),
        "delete\u{7f}".to_owned(),
        "bidi\u{202e}override".to_owned(),
        "C:evil".to_owned(),
        "a:b".to_owned(),
        "a<b".to_owned(),
        "a>b".to_owned(),
        "a\"b".to_owned(),
        "a|b".to_owned(),
        "a?b".to_owned(),
        "a*b".to_owned(),
        "session.".to_owned(),
        "session ".to_owned(),
        "CON".to_owned(),
        "prn.txt".to_owned(),
        "AUX".to_owned(),
        "NUL.json".to_owned(),
        "COM1".to_owned(),
        "com9.log".to_owned(),
        "LPT1".to_owned(),
        "lpt9.txt".to_owned(),
        "COM¹".to_owned(),
        "com³.log".to_owned(),
        "LPT²".to_owned(),
        "lpt³.txt".to_owned(),
        "AUX .txt".to_owned(),
        "com1 .log".to_owned(),
        "a".repeat(MAX_SESSION_ID_BYTES + 1),
    ] {
        assert!(
            matches!(
                SessionId::new(&*rejected),
                Err(StoreError::InvalidSessionId { .. })
            ),
            "{rejected:?} must be rejected"
        );
    }

    for accepted in ["COM0", "COM10", "LPT0", "LPT10", "console", "null"] {
        assert_eq!(SessionId::new(accepted).unwrap().as_ref(), accepted);
    }
}

#[test]
fn enum_text_canonicalizes_and_unknown_values_round_trip() {
    assert_eq!(MemberKind::from_raw("build").unwrap(), MemberKind::Build);
    assert_eq!(
        MemberKind::from_raw("conversation").unwrap(),
        MemberKind::Conversation
    );
    assert_eq!(
        MemberOrigin::from_raw("local").unwrap(),
        MemberOrigin::Local
    );
    assert_eq!(
        MemberOrigin::from_raw("remote").unwrap(),
        MemberOrigin::Remote
    );
    assert_eq!(Grouping::from_raw("state").unwrap(), Grouping::State);
    assert_eq!(
        Grouping::from_raw("directory").unwrap(),
        Grouping::Directory
    );

    let unknown_kind = MemberKind::from_raw("future-kind").unwrap();
    assert!(matches!(&unknown_kind, MemberKind::Other(v) if v.as_ref() == "future-kind"));
    assert_eq!(unknown_kind.as_str(), "future-kind");
    assert_eq!(unknown_kind, MemberKind::from_raw("future-kind").unwrap());

    let unknown_origin = MemberOrigin::from_raw("future-origin").unwrap();
    assert_eq!(unknown_origin.as_str(), "future-origin");
    let unknown_grouping = Grouping::from_raw("future-grouping").unwrap();
    assert_eq!(unknown_grouping.as_str(), "future-grouping");

    for empty in [
        MemberKind::from_raw("").map(drop),
        MemberOrigin::from_raw("").map(drop),
        Grouping::from_raw("").map(drop),
    ] {
        assert!(matches!(
            empty,
            Err(StoreError::InvalidEnumValue {
                reason: "empty",
                ..
            })
        ));
    }

    let over_cap = "x".repeat(MAX_ENUM_BYTES + 1);
    assert!(matches!(
        MemberKind::from_raw(&over_cap),
        Err(StoreError::EnumValueTooLong {
            column: "kind",
            max: MAX_ENUM_BYTES
        })
    ));
    assert!(matches!(
        MemberOrigin::from_raw(&over_cap),
        Err(StoreError::EnumValueTooLong {
            column: "origin",
            max: MAX_ENUM_BYTES
        })
    ));
    assert!(matches!(
        Grouping::from_raw(&over_cap),
        Err(StoreError::EnumValueTooLong {
            column: "grouping",
            max: MAX_ENUM_BYTES
        })
    ));
}

#[test]
fn metadata_validation_enforces_cwd_and_truncates_display_text() {
    let metadata = MemberMetadata {
        cwd: Some("/work/project".to_owned()),
        // 'é' is two bytes and starts on the cap boundary, so a byte-index cut would split the char
        title: Some(format!("{}é tail", "t".repeat(MAX_TITLE_BYTES - 1))),
        model: Some("m".repeat(MAX_MODEL_BYTES + 5)),
        last_turn_summary: Some("s".repeat(MAX_SUMMARY_BYTES + 5)),
        is_worktree: true,
        last_change_unix_ms: 42,
    };
    let expected_title = "t".repeat(MAX_TITLE_BYTES - 1);
    let expected_model = "m".repeat(MAX_MODEL_BYTES);
    let expected_summary = "s".repeat(MAX_SUMMARY_BYTES);
    assert_eq!(
        metadata.validated(&MemberKind::Build).unwrap(),
        ValidatedMetadataRef {
            cwd: Some("/work/project"),
            title: Some(expected_title.as_str()),
            model: Some(expected_model.as_str()),
            last_turn_summary: Some(expected_summary.as_str()),
            is_worktree: true,
            last_change_unix_ms: 42,
        }
    );

    let without_cwd = MemberMetadata {
        cwd: None,
        title: None,
        model: None,
        last_turn_summary: None,
        is_worktree: false,
        last_change_unix_ms: 0,
    };
    assert!(matches!(
        without_cwd.validated(&MemberKind::Build),
        Err(StoreError::CwdRequired)
    ));
    assert_eq!(
        without_cwd
            .validated(&MemberKind::Conversation)
            .unwrap()
            .cwd,
        None
    );

    let relative = MemberMetadata {
        cwd: Some("relative/path".to_owned()),
        ..without_cwd.clone()
    };
    for kind in [MemberKind::Build, MemberKind::Conversation] {
        assert!(matches!(
            relative.validated(&kind),
            Err(StoreError::CwdNotAbsolute)
        ));
    }

    let too_long = MemberMetadata {
        cwd: Some(format!("/{}", "c".repeat(MAX_CWD_BYTES))),
        title: None,
        model: None,
        last_turn_summary: None,
        is_worktree: false,
        last_change_unix_ms: 0,
    };
    assert!(matches!(
        too_long.validated(&MemberKind::Build),
        Err(StoreError::CwdTooLong { max: MAX_CWD_BYTES })
    ));
}
