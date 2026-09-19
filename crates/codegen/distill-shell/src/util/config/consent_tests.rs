// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use toml::Value as TomlValue;

/// A single slot would drop one of two live notices and re-prompt forever.
#[test]
fn consent_answers_are_kept_per_notice() {
    let root: TomlValue = toml::from_str(
        r#"
[consent.answers."enterprise-tos-2026-08"]
version = 2
account = "user@example.com"

[consent.answers."consumer-tos-2026-08"]
version = 1
account = "other@example.com"
"#,
    )
    .unwrap();

    let consent = super::super::load_config_from_toml(&root).consent;

    assert_eq!(
        consent
            .answers
            .get("enterprise-tos-2026-08")
            .map(|a| a.version),
        Some(2)
    );
    assert_eq!(
        consent
            .answers
            .get("consumer-tos-2026-08")
            .and_then(|a| a.account.as_deref()),
        Some("other@example.com")
    );

    let emitted = toml::to_string(&consent).unwrap();
    let reparsed: ConsentConfig = toml::from_str(&emitted).unwrap();
    assert_eq!(reparsed, consent);
}

#[tokio::test]
#[serial_test::serial(GROK_HOME)]
async fn set_consent_answer_is_monotonic_per_account() {
    let home = tempfile::tempdir().expect("home");
    let _guard = distill_test_support::env::EnvGuard::set("GROK_HOME", home.path());

    let answers = || {
        // Persist writes live `$GROK_HOME`. Read that dest; `load_from_disk` must
        // match it (a OnceLock miss used to look like a stale replay lowered the record).
        let path = super::super::user_config_path();
        let (dest, _) = super::super::read_follow_bound(&path).expect("bind persist dest");
        let persist_root =
            crate::config::load_config_file(dest.as_path()).expect("read persist dest");
        let persist = super::super::load_config_from_toml(&persist_root)
            .consent
            .answers;
        let disk_root = crate::config::load_from_disk().expect("read config");
        let disk = super::super::load_config_from_toml(&disk_root)
            .consent
            .answers;
        assert_eq!(
            disk, persist,
            "load_from_disk must see the consent persist wrote"
        );
        persist
    };

    set_consent_answer(Some("a@example.com".into()), "tos".into(), 3, false)
        .await
        .expect("first answer");
    set_consent_answer(Some("a@example.com".into()), "tos".into(), 1, false)
        .await
        .expect("replayed answer");
    assert_eq!(
        answers().get("tos").map(|a| a.version),
        Some(3),
        "a stale replay must not lower the record",
    );

    set_consent_answer(Some("a@example.com".into()), "tos".into(), 4, true)
        .await
        .expect("server ack");
    assert!(
        answers().get("tos").is_some_and(|a| a.acked),
        "the ack must reach the record"
    );

    set_consent_answer(Some("a@example.com".into()), "tos".into(), 1, false)
        .await
        .expect("replay after the ack");
    let Some(entry) = answers().get("tos").cloned() else {
        panic!("expected tos consent answer");
    };
    assert_eq!(entry.version, 4);
    assert!(
        entry.acked,
        "a replay must not unset the ack it did not make"
    );

    // The local write and the server ack race for one version, and the local one carries `false`.
    set_consent_answer(Some("a@example.com".into()), "tos".into(), 4, false)
        .await
        .expect("the slower local write");
    assert!(
        answers().get("tos").is_some_and(|a| a.acked),
        "the slower writer must not retract the ack"
    );

    set_consent_answer(Some("b@example.com".into()), "tos".into(), 1, false)
        .await
        .expect("second account");
    let Some(entry) = answers().get("tos").cloned() else {
        panic!("expected tos consent answer");
    };
    assert_eq!(entry.version, 1, "a different account starts over");
    assert_eq!(entry.account.as_deref(), Some("b@example.com"));
    assert!(
        !entry.acked,
        "the ack belongs to the answer it was made for"
    );

    set_consent_answer(None, "tos".into(), 2, false)
        .await
        .expect("signed-out answer");
    assert_eq!(
        answers().get("tos").and_then(|a| a.account.clone()),
        None,
        "a signed-out answer must not read back as the previous account",
    );

    set_consent_answer(Some("b@example.com".into()), "aup".into(), 2, false)
        .await
        .expect("second notice");
    assert_eq!(
        answers().len(),
        2,
        "answering a second notice must not evict the first",
    );
}
