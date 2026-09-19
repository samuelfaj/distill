// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::{BTreeSet, HashMap};

use pretty_assertions::assert_eq;
use distill_feedback::{
    FEEDBACK_DRAFTS_FILENAME, FeedbackDraftInput, FeedbackDraftStore, FeedbackFailureMode,
    FeedbackStoreError, FeedbackTaskCategory, FeedbackType,
};

use super::*;
use crate::types::resources::{Resources, SessionFolder};
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::ToolKind;
use crate::types::tool_metadata::{ToolMetadata as _, test_ctx};

fn input(details: &str, failure_mode: Option<FeedbackFailureMode>) -> FeedbackDraftInput {
    FeedbackDraftInput {
        title: "Draft title".to_owned(),
        details: details.to_owned(),
        area: None,
        r#type: FeedbackType::Bug,
        task_category: Some(FeedbackTaskCategory::Debug),
        failure_mode,
    }
}

fn tool_input(input: FeedbackDraftInput) -> SendFeedbackInput {
    SendFeedbackInput {
        title: input.title,
        details: input.details,
        area: input.area,
        r#type: input.r#type,
        task_category: input.task_category,
        failure_mode: input.failure_mode,
        draft_id: None,
    }
}

async fn run_tool(
    session_folder: &std::path::Path,
    input: SendFeedbackInput,
) -> Result<SendFeedbackOutput, distill_tool_runtime::ToolError> {
    let mut resources = Resources::new();
    resources.insert(SessionFolder(session_folder.to_path_buf()));
    distill_tool_runtime::Tool::run(&SendFeedbackTool, test_ctx(resources.into_shared()), input).await
}

async fn run(
    session_folder: &std::path::Path,
    input: FeedbackDraftInput,
) -> Result<SendFeedbackOutput, distill_tool_runtime::ToolError> {
    run_tool(session_folder, tool_input(input)).await
}

#[tokio::test]
async fn valid_inputs_append_canonical_drafts_and_omit_absent_optionals() {
    let session = tempfile::tempdir().unwrap();
    let full = FeedbackDraftInput {
        title: "Wrong API used".to_owned(),
        details: "What happened:\n- The edit used the wrong API.".to_owned(),
        area: Some("editing".to_owned()),
        r#type: FeedbackType::Bug,
        task_category: Some(FeedbackTaskCategory::CodeEdit),
        failure_mode: Some(FeedbackFailureMode::Hallucinated),
    };
    let mut sparse = input("What happened:\n- A useful idea.", None);
    sparse.task_category = None;

    let output = run(session.path(), full.clone()).await.unwrap();
    assert_eq!(output.message, SUCCESS_MESSAGE);
    run(session.path(), sparse.clone()).await.unwrap();

    let stored: Vec<(FeedbackDraftInput, u64)> = FeedbackDraftStore::new(session.path())
        .list()
        .unwrap()
        .into_iter()
        .map(|draft| {
            (
                FeedbackDraftInput {
                    title: draft.title,
                    details: draft.details,
                    area: draft.area,
                    r#type: draft.r#type.unwrap(),
                    task_category: draft.task_category,
                    failure_mode: draft.failure_mode,
                },
                draft.revision,
            )
        })
        .collect();
    assert_eq!(stored, [(full, 1), (sparse, 1)]);
}

#[tokio::test]
async fn crate_validation_maps_to_invalid_arguments_without_writing() {
    for invalid in [
        FeedbackDraftInput {
            title: " \n ".to_owned(),
            ..input("details", None)
        },
        input(" \n ", None),
        input(&"x".repeat(64 * 1024 + 1), None),
    ] {
        let session = tempfile::tempdir().unwrap();
        let error = run(session.path(), invalid).await.unwrap_err();

        assert_eq!(
            error.kind,
            distill_tool_runtime::ToolErrorKind::InvalidArguments
        );
        assert!(!session.path().join(FEEDBACK_DRAFTS_FILENAME).exists());
    }
}

#[tokio::test]
async fn storage_and_join_failures_are_execution_errors() {
    let storage_error = map_append_result(Ok(Err(FeedbackStoreError::Busy))).unwrap_err();
    assert_eq!(
        storage_error.kind,
        distill_tool_runtime::ToolErrorKind::Execution
    );
    assert!(!storage_error.detail.contains(SUCCESS_MESSAGE));

    let join_error = tokio::task::spawn_blocking(|| panic!("test panic"))
        .await
        .unwrap_err();
    let join_error = map_append_result(Err(join_error)).unwrap_err();
    assert_eq!(join_error.kind, distill_tool_runtime::ToolErrorKind::Execution);
    assert!(!join_error.detail.contains(SUCCESS_MESSAGE));
}

#[tokio::test]
async fn draft_id_updates_existing_draft_instead_of_appending() {
    let session = tempfile::tempdir().unwrap();
    let existing = FeedbackDraftStore::new(session.path())
        .append_predraft("Todo list", "todos are chopped")
        .unwrap();

    let mut update = tool_input(input("What happened:\n- Todos wrap.", None));
    update.draft_id = Some(existing.id.as_str().to_owned());
    update.r#type = FeedbackType::Bug;
    update.task_category = Some(FeedbackTaskCategory::Debug);
    update.failure_mode = Some(FeedbackFailureMode::Other);

    let output = run_tool(session.path(), update).await.unwrap();

    assert_eq!(output.message, SUCCESS_MESSAGE);
    let drafts = FeedbackDraftStore::new(session.path()).list().unwrap();
    let [draft] = drafts.as_slice() else {
        panic!("expected exactly one draft: {drafts:?}");
    };
    assert_eq!(draft.id, existing.id);
    assert_eq!(draft.r#type, Some(FeedbackType::Bug));
    assert_eq!(draft.task_category, Some(FeedbackTaskCategory::Debug));
    assert_eq!(draft.failure_mode, Some(FeedbackFailureMode::Other));
    assert_eq!(draft.revision, 2);
}

#[tokio::test]
async fn draft_id_update_keeps_blank_lines_the_model_wrote() {
    let session = tempfile::tempdir().unwrap();
    let existing = FeedbackDraftStore::new(session.path())
        .append_predraft("Todo list", "todos are chopped")
        .unwrap();

    let details = "What happened:\n- Todos wrap.\n\nRepro:\n- Open /todo";
    let mut update = tool_input(input(details, None));
    update.draft_id = Some(existing.id.as_str().to_owned());

    run_tool(session.path(), update).await.unwrap();

    let drafts = FeedbackDraftStore::new(session.path()).list().unwrap();
    let [draft] = drafts.as_slice() else {
        panic!("expected exactly one draft: {drafts:?}");
    };
    assert_eq!(draft.details, details);
}

#[test]
fn strip_draft_id_drops_only_the_line_the_id_filled() {
    let id = "0195a4e2-draft";
    for (text, expected) in [
        ("See 0195a4e2-draft here", "See  here"),
        (
            "What happened:\n0195a4e2-draft\n\n- Todos wrap.",
            "What happened:\n\n- Todos wrap.",
        ),
        (
            "What happened:\n- Todos wrap.\n0195a4e2-draft",
            "What happened:\n- Todos wrap.",
        ),
        (
            "What happened:\n0195a4e2-draft\n- Todos wrap.\n",
            "What happened:\n- Todos wrap.\n",
        ),
    ] {
        assert_eq!(strip_draft_id(text, id), expected, "{text:?}");
    }
}

#[tokio::test]
async fn missing_draft_id_does_not_append() {
    let session = tempfile::tempdir().unwrap();
    let mut update = tool_input(input("What happened:\n- Missing.", None));
    update.draft_id = Some("does-not-exist".to_owned());

    let error = run_tool(session.path(), update).await.unwrap_err();

    assert_eq!(
        error.kind,
        distill_tool_runtime::ToolErrorKind::InvalidArguments
    );
    assert!(
        FeedbackDraftStore::new(session.path())
            .list()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn schema_has_canonical_fields() {
    let schema = crate::registry::types::generate_schema::<super::SendFeedbackInput>();
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        panic!("schema missing properties: {schema}");
    };
    assert_eq!(
        properties
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "area",
            "details",
            "draft_id",
            "failure_mode",
            "task_category",
            "title",
            "type"
        ]),
    );
    let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
        panic!("schema missing required: {schema}");
    };
    assert_eq!(
        required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["details", "title", "type"]),
    );
}

#[test]
fn description_template_keeps_real_delimiters() {
    let template = SendFeedbackTool.description_template();
    assert!(template.contains("${{ params.feedback.draft_id }}"));
    assert!(template.contains("${{ params.feedback.title }}"));
    assert!(template.contains("${{ params.feedback.details }}"));
    assert!(template.contains("${{ params.feedback.area }}"));
    assert!(template.contains("${{ params.feedback.failure_mode }}"));
    assert!(template.contains("${%- if tools.by_kind.ask_user %}"));
    assert!(template.contains("${{ tools.by_kind.ask_user }}"));
    assert!(template.contains("${{ feedback_drafts_path }}"));
    assert!(!template.contains("${ params."));
    assert!(!template.contains("${- if"));
}

fn feedback_renderer(tool_names: HashMap<ToolKind, String>) -> TemplateRenderer {
    TemplateRenderer::new(
        tool_names,
        HashMap::from([(
            ToolKind::Feedback,
            HashMap::from([
                ("draft_id".to_owned(), "draft_id".to_owned()),
                ("title".to_owned(), "title".to_owned()),
                ("details".to_owned(), "details".to_owned()),
                ("area".to_owned(), "area".to_owned()),
                ("failure_mode".to_owned(), "failure_mode".to_owned()),
            ]),
        )]),
    )
}

fn advertised_description(renderer: &TemplateRenderer) -> String {
    crate::types::tool_metadata::ToolMetadata::versioned_definition(
        &SendFeedbackTool,
        None,
        SEND_FEEDBACK_TOOL_NAME,
        None,
        renderer,
        &HashMap::new(),
        &serde_json::json!({"type": "object", "properties": {}}),
        &serde_json::json!({}),
    )
    .function
    .description
    .expect("description")
}

#[test]
fn advertised_description_renders_present_names_and_omits_absent_ones() {
    let drafts = drafts_file_path(std::path::Path::new("/tmp/session"));
    let text = advertised_description(
        &feedback_renderer(HashMap::from([
            (ToolKind::Feedback, "send_feedback".to_owned()),
            (ToolKind::AskUser, "ask_user_question".to_owned()),
        ]))
        .with_feedback_drafts_path(&drafts),
    );
    for name in ["draft_id", "title", "details", "area", "failure_mode"] {
        assert!(text.contains(name), "{name} missing from {text}");
    }
    assert!(text.contains("ask_user_question"), "{text}");
    assert!(text.contains(&drafts), "{text}");
    assert!(!text.contains("${"), "{text}");

    let text = advertised_description(&feedback_renderer(HashMap::from([(
        ToolKind::Feedback,
        "send_feedback".to_owned(),
    )])));
    assert!(text.contains("draft_id"), "{text}");
    assert!(!text.contains("ask_user"), "{text}");
    assert!(!text.contains("drafts file"), "{text}");
    assert!(!text.contains("${"), "{text}");
}

#[test]
fn leaked_one_brace_peel_is_stripped() {
    let peeled = "use the ${ params.feedback.draft_id } field\n${- if tools.by_kind.ask_user %}";
    let stripped = super::strip_leaked_template_syntax(peeled);
    assert!(!stripped.contains("${"));
    assert!(!stripped.contains("params.feedback"));
}
