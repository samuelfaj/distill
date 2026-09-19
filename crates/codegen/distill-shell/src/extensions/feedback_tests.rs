// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::extensions::feedback_drafts::{answer, draft_op_error};
use crate::session::FeedbackDraftUpdateRequest;
use distill_feedback::{
    FeedbackDraft, FeedbackDraftStore, FeedbackFailureMode, FeedbackStoreError,
    FeedbackTaskCategory, FeedbackType, UpdateOutcome,
};
use distill_telemetry::events::FeedbackDraftOpKind;

/// Wire spelling of the `FeedbackDraftUpdateRequest` the pager sends on `drafts/update`.
fn pager_update_body(draft_id: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": "sess-1",
        "draft_id": draft_id,
        "details": "edited body",
        "title": "edited title",
        "area": "editing",
        "type": "bug",
        "task_category": "debug",
        "failure_mode": "hallucinated",
    })
}

/// Wire spelling of the `FeedbackDraftSendRequest` the pager sends on `x.ai/feedback`.
fn pager_send_body() -> serde_json::Value {
    serde_json::json!({
        "session_id": "sess-1",
        "draft_id": "01931111-aaaa-7bbb-8ccc-ddddeeeeffff",
        "request_trace_upload_token": true,
        "edited_body": {
            "title": "edited title",
            "details": "edited body",
            "area": "editing",
            "type": "bug",
            "task_category": "debug",
            "failure_mode": "hallucinated",
            "images": [{ "data": "aGk=", "mimeType": "image/png" }],
            "client_version": "1.2.3",
            "terminal_info": {
                "brand": "Ghostty",
                "multiplexer": "tmux",
                "isSsh": false,
                "isByobu": false,
                "termVar": "xterm-ghostty",
            },
        },
    })
}

fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(
    body: &serde_json::Value,
) -> serde_json::Value {
    serde_json::to_value(serde_json::from_value::<T>(body.clone()).unwrap()).unwrap()
}

#[test]
fn drafts_send_request_round_trips_the_pager_body() {
    let full = pager_send_body();
    assert_eq!(round_trip::<FeedbackDraftSendRequest>(&full), full);

    // Absent optionals must round-trip as absent, not `null`.
    let mut minimal = full;
    let Some(edited_body) = minimal
        .get_mut("edited_body")
        .and_then(|v| v.as_object_mut())
    else {
        panic!("edited_body missing: {minimal:?}");
    };
    for optional in [
        "area",
        "task_category",
        "failure_mode",
        "client_version",
        "terminal_info",
    ] {
        edited_body.remove(optional);
    }
    assert_eq!(round_trip::<FeedbackDraftSendRequest>(&minimal), minimal);
}

#[test]
fn drafts_update_request_round_trips_the_pager_body() {
    let full = pager_update_body("01931111-aaaa-7bbb-8ccc-ddddeeeeffff");
    assert_eq!(round_trip::<FeedbackDraftUpdateRequest>(&full), full);

    let mut minimal = full;
    let body = minimal.as_object_mut().unwrap();
    for optional in ["area", "task_category", "failure_mode"] {
        body.remove(optional);
    }
    assert_eq!(round_trip::<FeedbackDraftUpdateRequest>(&minimal), minimal);
}

#[test]
fn drafts_update_writes_the_full_pager_body() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let predraft = store
        .append_predraft("Todo list", "todos are chopped")
        .unwrap();
    let request: FeedbackDraftUpdateRequest =
        serde_json::from_value(pager_update_body(predraft.id.as_str())).unwrap();

    assert_eq!(
        store
            .update_from_input(&request.draft_id, request.input)
            .unwrap(),
        UpdateOutcome::Updated
    );

    let stored = store
        .get(&predraft.id)
        .unwrap()
        .expect("draft still present");
    assert_eq!(
        stored,
        FeedbackDraft {
            title: "edited title".to_owned(),
            details: "edited body".to_owned(),
            area: Some("editing".to_owned()),
            r#type: Some(FeedbackType::Bug),
            task_category: Some(FeedbackTaskCategory::Debug),
            failure_mode: Some(FeedbackFailureMode::Hallucinated),
            revision: predraft.revision + 1,
            ..predraft
        }
    );
}

/// Pins the `feedback_draft_op` builder: the `list` count rides on success and a store failure
/// rides as its variant class only, never its message.
#[test]
fn draft_op_event_places_count_and_error_class() {
    let cases = [
        (
            FeedbackDraftOpKind::List,
            Ok(Some(2)),
            serde_json::json!({
                "session_id": "sess-1", "op": "list", "ok": true, "draft_count": 2
            }),
        ),
        (
            FeedbackDraftOpKind::Recover,
            Err(draft_op_error(&FeedbackStoreError::Busy)),
            serde_json::json!({
                "session_id": "sess-1", "op": "recover", "ok": false, "error": "busy"
            }),
        ),
        (
            FeedbackDraftOpKind::Delete,
            Ok(None),
            serde_json::json!({ "session_id": "sess-1", "op": "delete", "ok": true }),
        ),
    ];
    for (op, outcome, expected) in cases {
        assert_eq!(
            serde_json::to_value(feedback_drafts::draft_op_event("sess-1", op, outcome)).unwrap(),
            expected,
            "{op:?}"
        );
    }
}

#[test]
fn drafts_update_without_type_fails_the_parse_instead_of_half_updating() {
    let mut body = pager_update_body("01931111-aaaa-7bbb-8ccc-ddddeeeeffff");
    body.as_object_mut().unwrap().remove("type");

    let error = serde_json::from_value::<FeedbackDraftUpdateRequest>(body)
        .expect_err("a body without `type` must not parse");
    assert!(
        error.to_string().contains("missing field `type`"),
        "{error}"
    );
}

/// An `ExtRequest` for `method` whose params hold only `session_id`.
fn drafts_request(method: &str) -> acp::ExtRequest {
    let params = serde_json::json!({ "session_id": "sess-1" });
    acp::ExtRequest::new(
        method,
        serde_json::value::to_raw_value(&params)
            .expect("params serialize")
            .into(),
    )
}

#[tokio::test]
async fn answer_lists_drafts_and_refuses_an_unknown_method() {
    let session = tempfile::tempdir().expect("tempdir");
    let store = FeedbackDraftStore::new(session.path());
    store
        .append_predraft("Todo list", "todos are chopped")
        .expect("predraft appended");

    let listed = answer(
        &drafts_request("x.ai/feedback/drafts/list"),
        store.clone(),
        false,
    )
    .await
    .expect("list answers");
    let listed: serde_json::Value =
        serde_json::from_str(listed.0.get()).expect("list response is JSON");
    assert_eq!(
        listed
            .get("drafts")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );

    let unknown = answer(&drafts_request("x.ai/feedback/drafts/rename"), store, false)
        .await
        .expect_err("an unknown drafts method is refused");
    assert_eq!(unknown.code, acp::Error::method_not_found().code);
}

#[tokio::test]
async fn draft_send_input_returns_the_draft_text_and_its_id() {
    let session = tempfile::tempdir().expect("tempdir");
    let store = FeedbackDraftStore::new(session.path());
    let predraft = store
        .append_predraft("Todo list", "todos are chopped")
        .expect("predraft appended");
    let mut body = pager_send_body();
    body.as_object_mut()
        .expect("send body is an object")
        .insert("draft_id".to_owned(), predraft.id.to_string().into());

    let (input, (_, draft_id)) = draft_send_input(
        parse_draft_send_request(body).expect("send body parses"),
        store,
    )
    .await
    .expect("a present draft resolves");

    assert!(
        input
            .feedback_text
            .is_some_and(|text| text.contains("edited title"))
    );
    assert_eq!(draft_id, predraft.id);
}

#[tokio::test]
async fn draft_send_input_refuses_a_draft_the_store_does_not_have() {
    let session = tempfile::tempdir().expect("tempdir");
    let store = FeedbackDraftStore::new(session.path());

    let error = draft_send_input(
        parse_draft_send_request(pager_send_body()).expect("send body parses"),
        store,
    )
    .await
    .expect_err("a draft the store lacks is refused");

    assert_eq!(error.code, acp::Error::invalid_params().code);
}
