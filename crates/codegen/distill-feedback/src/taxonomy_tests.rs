use serde::de::DeserializeOwned;

use super::*;

/// Every variant's wire string in declaration order, each checked to deserialize back to itself.
fn wire_spellings<T>() -> Vec<String>
where
    T: IntoEnumIterator + Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    T::iter()
        .map(|variant| {
            let wire = serde_json::to_value(&variant).unwrap();
            assert_eq!(serde_json::from_value::<T>(wire.clone()).unwrap(), variant);
            wire.as_str().unwrap().to_owned()
        })
        .collect()
}

/// The `enum` array the tool schema advertises to the model; schemars spells variants independently of serde.
fn schema_enum<T: schemars::JsonSchema>() -> Vec<String> {
    let schema = schemars::schema_for!(T).to_value();
    schema
        .get("enum")
        .and_then(|e| e.as_array())
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

/// The v1 wire spellings are frozen (downstream consumers allowlist these exact strings); a
/// variant rename must not silently change the wire, and the tool schema must offer the same values.
#[test]
fn enum_wire_spellings_are_the_frozen_v1_values() {
    assert_eq!(wire_spellings::<FeedbackSource>(), ["write", "draft"]);

    let types = wire_spellings::<FeedbackType>();
    assert_eq!(types, ["bug", "idea", "missing_capability"]);
    assert_eq!(schema_enum::<FeedbackType>(), types);

    let tasks = wire_spellings::<FeedbackTaskCategory>();
    assert_eq!(
        tasks,
        [
            "code_edit",
            "debug",
            "explain",
            "plan",
            "shell",
            "search",
            "review",
            "other"
        ]
    );
    assert_eq!(schema_enum::<FeedbackTaskCategory>(), tasks);

    let failures = wire_spellings::<FeedbackFailureMode>();
    assert_eq!(
        failures,
        [
            "overeager",
            "stopped_early",
            "unwanted_scope",
            "didnt_ask_for_help",
            "excessive_questions",
            "subagent_overspawn",
            "over_correction",
            "ignored_instructions",
            "hallucinated",
            "sloppy_code",
            "destructive",
            "lost_context",
            "stuck_in_a_loop",
            "model_regression",
            "disputed",
            "wrong_tone",
            "unclear_output",
            "other",
        ]
    );
    assert_eq!(schema_enum::<FeedbackFailureMode>(), failures);
}

#[test]
fn failure_mode_accepts_the_old_wire_aliases() {
    let cases = [
        ("did_too_much", FeedbackFailureMode::Overeager),
        ("gave_up_early", FeedbackFailureMode::StoppedEarly),
        (
            "ignored_direction",
            FeedbackFailureMode::IgnoredInstructions,
        ),
        ("wrong_or_made_up", FeedbackFailureMode::Hallucinated),
        ("broke_something", FeedbackFailureMode::Destructive),
        ("stuck_in_loop", FeedbackFailureMode::StuckInALoop),
    ];

    for (alias, expected) in cases {
        assert_eq!(
            serde_json::from_value::<FeedbackFailureMode>(serde_json::json!(alias)).unwrap(),
            expected,
            "{alias}"
        );
    }
}

#[test]
fn structured_feedback_envelope_and_omissions() {
    let full = FeedbackTaxonomy {
        r#type: Some(FeedbackType::Bug),
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode: Some(FeedbackFailureMode::SloppyCode),
    };
    assert_eq!(
        structured_feedback(FeedbackSource::Draft, full),
        serde_json::json!({
            "structured_feedback": {
                "schema_version": 1,
                "source": "draft",
                "type": "bug",
                "task_category": "debug",
                "failure_mode": "sloppy_code",
            }
        })
    );

    // Absent enums are omitted outright: no JSON null placeholders.
    let partial = FeedbackTaxonomy {
        r#type: Some(FeedbackType::Idea),
        ..FeedbackTaxonomy::default()
    };
    assert_eq!(
        structured_feedback(FeedbackSource::Write, partial),
        serde_json::json!({
            "structured_feedback": {
                "schema_version": 1,
                "source": "write",
                "type": "idea",
            }
        })
    );

    // No enum at all still yields the envelope: the read side needs `source` on every send.
    assert_eq!(
        structured_feedback(FeedbackSource::Write, FeedbackTaxonomy::default()),
        serde_json::json!({
            "structured_feedback": { "schema_version": 1, "source": "write" }
        })
    );
}

#[test]
fn parse_structured_feedback_reads_the_envelope_and_drops_only_unknown_values() {
    let full = FeedbackTaxonomy {
        r#type: Some(FeedbackType::Bug),
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode: Some(FeedbackFailureMode::SloppyCode),
    };
    assert_eq!(
        parse_structured_feedback(Some(&structured_feedback(FeedbackSource::Draft, full))),
        Some(StructuredFeedback {
            source: Some(FeedbackSource::Draft),
            taxonomy: full,
        })
    );

    // A newer client's unknown values drop those fields alone, `source` included.
    let newer = serde_json::json!({
        "structured_feedback": {
            "schema_version": 1,
            "source": "voice",
            "type": "bug",
            "task_category": "not_a_category",
            "failure_mode": "sloppy_code",
        }
    });
    assert_eq!(
        parse_structured_feedback(Some(&newer)),
        Some(StructuredFeedback {
            source: None,
            taxonomy: FeedbackTaxonomy {
                r#type: Some(FeedbackType::Bug),
                task_category: None,
                failure_mode: Some(FeedbackFailureMode::SloppyCode),
            },
        })
    );

    let rejected = [
        serde_json::json!({ "team": "platform-tools" }),
        serde_json::json!({ "structured_feedback": { "schema_version": 2, "source": "write" } }),
    ];
    for metadata in &rejected {
        assert_eq!(
            parse_structured_feedback(Some(metadata)),
            None,
            "{metadata}"
        );
    }
}
